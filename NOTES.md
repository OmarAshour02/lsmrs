# Notes

TILs, gotchas, and book connections collected while building lsmrs. Design
*decisions* live in DESIGN.md; this file is the reasoning and the tangents.

---

## Phase 4 — Bloom Filters

### The filter is a miss-path optimization only

`SSTable::get` already rejects on `min_key`/`max_key`. That check is nearly
worthless in the realistic case: with scattered writes every flushed table
spans most of the keyspace, so every table answers "maybe" and the read pays a
`File::open` + seek + block scan *per table* to learn nothing.

A read of a key that **exists** gains nothing from a bloom filter — it still
does all the work, plus the probe. That asymmetry is the whole design
constraint: the filter must be cheap enough that hits don't notice it, and it
must be RAM-resident, because a filter you read from disk has already lost the
race it existed to win.

### False positives are a budget; false negatives are fatal

A false positive costs exactly one wasted block read — the work we do today
anyway. A false negative would report a live key as missing. Bloom filters
can't produce false negatives *as long as nothing is ever removed from them*,
which is why a bloom filter has no `remove`.

Consequence that's easy to miss: **tombstones must be inserted into the filter
too.** A delete is a record like any other. If a newer table's tombstone isn't
findable, the read falls through to an older table and resurrects a deleted
value.

### Sizing: we know `n` exactly

Most bloom filter code has to guess how many keys it will hold. We don't — at
flush time `memtable.len()` is exact. So:

- `m` (bits)   = `n * bits_per_key`
- `k` (probes) = `round(ln2 * bits_per_key)`

At 10 bits/key: `k = 7`, false positive rate ≈ 1%. That turns "10,000 of 10,000
misses do wasted work" into "~80 of 10,000".

### Derived parameters belong in the file, not in Config

If the reader recomputes `k` from `Config`, then editing `bits_per_key` later
silently corrupts every SSTable already on disk: same bits, different probe
count, wrong answers, no error.

`bits_per_key` is **write-time policy**. `m` and `k` are **properties of the
bytes**. So they're serialized into the filter block and the reader trusts the
file. This also unblocks Phase 5, where compaction rebuilds filters and may
legitimately pick different parameters per file.

### Do not use `DefaultHasher` for anything persisted

`std::collections::hash_map::DefaultHasher`'s docs state the algorithm is
unspecified and may change between Rust releases. Fine for an in-memory
`HashMap`; fatal here — filters written under one toolchain, read under
another, produce silent false negatives for some keys.

The general rule, worth carrying past this project: *anything that touches a
persisted byte must have a pinned algorithm.* Same reason the WAL names CRC32
explicitly rather than "some checksum."

So we write our own hash. FNV-1a 64-bit is ~8 lines and stable by definition,
but its low bits avalanche poorly — and we take `% m`, which reads exactly
those bits. A splitmix-style finalizer fixes that in ~4 more lines. (xxHash64
is better quality but 60 lines of constants and no more instructive.)

### One hash, not `k` of them — Kirsch–Mitzenmacher

`k = 7` naively means seven hash computations per probe. Instead: take one
64-bit hash, split it into two 32-bit halves `h1`/`h2`, and generate probe `i`
as `h1 + i*h2`. Provably no worse asymptotically than `k` independent hashes.
LevelDB and RocksDB both do this. One hash of the key, then seven cheap adds.

### Format versioning

Adding the filter grows the trailer, so every pre-existing `.sst` file becomes
garbage that `SSTable::open` will happily read as valid — whatever bytes sit at
the trailer offsets get taken as offsets. The format has no magic number and no
version byte, so there's nothing to fail on.

### DDIA connection

Ch. 4 introduces bloom filters exactly here: as the thing that saves the LSM
read path from paying for every level on a miss. The book's framing — LSM trees
have great write amplification but a read must consult many segments — is the
problem this phase measures. The `read_path_overlapping/get_miss` benchmark is
that cost made visible.

### Measured false-positive rates

20,000 keys inserted, 80,000 absent keys probed:

| bits/key | k | measured FPR | theoretical | filter size |
|---|---|---|---|---|
| 4  | 3  | 14.77% | 14.7% | 9 KB  |
| 8  | 6  | 2.20%  | 2.1%  | 19 KB |
| 10 | 7  | 0.85%  | 0.82% | 24 KB |
| 16 | 11 | 0.04%  | 0.05% | 39 KB |

Measurement tracks theory to within a rounding error at every size, which is the
real verdict on FNV-1a + fmix64: a hash with poor avalanche would show measured
rates well *above* theory, because correlated strides make keys collide more
than the independent-probe math predicts.

10 bits/key is the knee. Going 8→10 costs 5KB per 20k keys and cuts the miss
rate by 2.6x; going 10→16 costs another 15KB for a further 20x, which only pays
off if wasted block reads are more expensive than RAM.

### `wrapping_*` is for intended wrapping, not defensive noise

First instinct was `wrapping_add`/`wrapping_mul` in the probe walk. Wrong at this
width: `start < 2^32`, `stride < 2^32`, `i <= 29`, so `start + i*stride < 2^37` —
overflow is impossible in `u64` and the plain operators are correct.

LevelDB *does* need wrapping there because it does the same arithmetic in
`uint32`, where it genuinely overflows and wrapping is the intended semantics.

The hash still uses `wrapping_mul` — there the wraparound *is* the algorithm.
The rule: reach for `wrapping_*` where wrapping is meant, not to silence a panic
that can't happen.

### Phase 4 benchmark: before / after

Baseline is a git worktree at `d9aecfa` (Phase 3), same machine, same session.
10,000 keys across ~13 SSTables, filters at 10 bits/key.

| benchmark | before | after | change |
|---|---|---|---|
| `read_path/get_miss` | 5.13 µs | 250 ns | 20x |
| `read_path_overlapping/get_miss` | 63.9 µs | 1.22 µs | 52x |
| `read_path_overlapping/get_hit` | 67.1 µs | 7.13 µs | 9.4x |
| `read_path/get_hit` | 6.23 µs | 6.06 µs | — |
| `put` | 3.26 µs | 3.14 µs | — |

**Hits improve too, which contradicts the framing this phase started from.** The
filter was described as a miss-path optimization, and that's only true for the
one table that actually holds the key. A hit in the overlapping workload was
opening and scanning roughly half of the 13 tables before reaching the right
one; those are now rejected by a probe. The correct statement is that the filter
eliminates *per-table* work for every table that lacks the key, whether or not
some other table has it.

`read_path/get_hit` is the control: sequential inserts give disjoint key ranges,
so min/max already narrowed the search to one table and there was nothing left
for the filter to skip. No change there confirms the win above is really the
filter and not measurement drift.

`put` is flat — seven probes per key at flush time vanish into the WAL write.

The 52x is against an unusually bad baseline: no block cache, and
`SSTable::get` opens the file with a fresh `File::open` on every lookup. A store
that kept file handles or cached blocks would show a smaller multiple.

## Phase 5 — Compaction

### The lock protects the list, not the files

SSTables are immutable, so two threads reading the same file is not a race and
needs no coordination at all. The compactor never touches its inputs -- it reads
them and writes a new file. The only shared mutable state is *which files exist*,
which is a `Vec` of a dozen pointers.

So the write lock is held for a pointer swap, not for the merge:

```
snapshot()          locked, one refcount bump
merge(...)          unlocked -- the slow part
write_table(...)    unlocked -- writes the output file
replace_tables()    locked, pointer swap
remove_file(...)    unlocked
```

A design that excluded readers during compaction would have been correct and
roughly 1000x worse on tail latency: reads are ~4 us, a merge is tens of ms.

### `unlink` is what makes deletion safe

Deleting an SSTable while a reader is mid-scan is fine on POSIX. `unlink`
removes a *directory entry* and decrements the inode's link count; the inode and
its blocks survive until the last open descriptor closes. A reader that opened
the file before the swap keeps reading it, intact, through a file that no longer
has a name.

This is why `SSTable` now holds its `File` open from `open()` onward rather than
calling `File::open` per lookup. Two payoffs from one change: hits got ~1.5x
faster, and deletion needs no reader coordination.

It also forced `read_exact_at` (`pread`) over `Read + Seek`. `read_at` takes
`&self`, because the offset is an argument rather than shared cursor state --
so any number of threads can read one descriptor with no lock. `Read`/`Seek`
take `&mut self` and would have forced a `Mutex<File>`, serialising every
reader. The concurrency design depended on picking the right I/O API.

### `RwLock<Arc<Vec<T>>>` beats `RwLock<Vec<T>>`

First attempt was `Arc<RwLock<Vec<Arc<SSTable>>>>`, where a reader clones the
`Vec` to get a snapshot. That regressed `get_miss` from 241ns to 434ns -- one
heap allocation plus a refcount bump per table, on every single read.

`Arc<RwLock<Arc<Vec<Arc<SSTable>>>>>` fixed it: the reader clones one `Arc`, a
single atomic increment, no allocation. The `Vec` is immutable once published;
a writer builds a fresh one and swaps the pointer. Cost moves to the writer,
which clones a 13-element `Vec` per flush -- the right trade when reads are
constant and flushes are rare.

This is the shape of RCU, and the lesson generalises: when readers vastly
outnumber writers, make the shared thing immutable and swap pointers.

### Crash recovery without a MANIFEST

Filenames carry the span of flushes a table contains --
`sstable-000002-000003.sst` is the merge of flushes 2 and 3. Two properties fall
out:

- **Ordering survives compaction.** Sorting by the first number keeps the merge
  where its inputs were, instead of letting it sort last and shadow newer data.
- **Leftovers identify themselves.** After a crash between "publish the output"
  and "delete the inputs", any file whose span is contained in another's is an
  orphan. `[2,2]` and `[3,3]` are inside `[2,3]`, so they go.

That second rule is only sound because of atomic publish: write to `.tmp`,
fsync, `rename`, fsync the directory. A file under its final name is therefore
always complete, so its presence is proof the merge finished. Without the
rename, a half-written merge would appear and recovery would delete the two good
inputs in its favour.

Cost: puts went from 3.14us to ~4.0us, all of it the directory fsync per flush.

### Tombstones can only be dropped by the oldest merge

Compaction reclaims space by discarding overwritten values and deleted keys.
Overwrites are free -- the newest wins and the rest vanish. Tombstones are not:
dropping one while an older table still holds the key resurrects the value.

So a merge may drop tombstones only when no live table is older than its inputs.
Third appearance of the same bug class this project, after "tombstones must go
in the bloom filter" and "a merge must not sort as newest": *an old value
becoming reachable again*.

### Shutdown is the absence of senders

`rx.recv()` returns `Err` once every `Sender` has dropped, so the compactor's
loop is just `while rx.recv().is_ok()`. No stop flag, no `AtomicBool`, no
poison message -- dropping the sender *is* the signal. `Db::drop` takes the
sender, drops it, then `join`s.

The `Option<Sender>` / `Option<JoinHandle>` fields exist because `Drop::drop`
gets `&mut self`, and both dropping a sender and joining a handle need to
*consume* the value. `Option::take` is the standard way out.

A pleasant side effect: `Drop` joining the compactor makes the background thread
*testable*. A test writes 20 tables, drops the `Db`, and by the time the scope
ends every triggered compaction has finished -- deterministic, no sleeps.

### Benchmark after compaction

| benchmark | Phase 4 | after Phase 5 |
|---|---|---|
| `read_path/get_miss` | 250 ns | 221 ns |
| `read_path/get_hit` | 6.06 us | 4.14 us |
| `read_path_overlapping/get_miss` | 1.22 us | 548 ns |
| `read_path_overlapping/get_hit` | 7.13 us | 4.94 us |
| `put` | 3.14 us | 3.96 us |

Hits gained from holding descriptors open. The overlapping miss halved again
because compaction cut the number of tables a lookup must probe. Puts pay the
directory fsync.

### Known limitation

`merge` materialises its whole output in a `Vec` before writing, because
`write_table` needs the key count up front to size the bloom filter. Inputs
stream; the output does not. Fine at 4MB tables, not at 400MB. The fix is a v2
format storing key count in the metadata block, so the sum of inputs gives an
upper bound.

## Phase 6 — Network Protocol

### A length prefix is an allocation instruction from a stranger

Every phase before this one parsed bytes it had written itself. A socket does
not: `$999999999\r\n` is a client asking the server to allocate a gigabyte
before a single byte of payload arrives. Hence `MAX_BULK_LEN`, checked *before*
`vec![0u8; len]`, and a test named after the ordering.

This is the first untrusted input in the project, and it is a different failure
class from anything the storage engine handles.

### Why RESP is length-prefixed and not delimited

`SET binary "a\r\nb"` works -- a value containing the protocol's own terminator
survives, because the bulk string says how many bytes to read instead of
scanning for a marker. Exactly the reasoning behind the SSTable record format
in Phase 3, arrived at independently by Redis for the same reason: the data is
arbitrary bytes and no byte can be reserved.

Inline commands (`GET key\r\n`, what netcat sends) are the exception and are
*not* binary safe. That is fine because they exist for humans.

### `Arc<RwLock<Db>>` and the write bottleneck

`Db::get` takes `&self` and `put`/`delete` take `&mut self`, so `RwLock` maps
onto the existing API with no changes: reads run concurrently, writes serialise.

Worth being honest that this is a real ceiling. Every `SET` in the process takes
one global exclusive lock, so write throughput does not scale with cores no
matter how many connection threads exist. Redis reaches the same place by a
different route (a single thread). Fixing it properly means sharding the
memtable or making the WAL append lock-free -- well past this project.

### `try_clone` is `dup(2)`

`BufReader::new(stream)` takes ownership of the socket, but the connection also
needs to write. `stream.try_clone()` returns a second `TcpStream` referring to
the same underlying descriptor -- `dup(2)` -- so the reader and writer halves
can be owned independently. Closing one does not close the connection; the
socket dies when the last handle drops.

### Waking a blocked `accept`

`TcpListener::accept` blocks, so an `AtomicBool` alone cannot stop the loop --
the flag is only checked between accepts, and there is no next accept until
someone connects.

The fix is to connect to our own port:

```rust
pub fn shutdown(&self) {
    self.flag.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect(self.addr);
}
```

Slightly grubby, entirely standard, and it makes the server testable -- the test
harness can stop the accept thread and `join` it instead of leaking one per
test. The alternative is non-blocking sockets plus a poll loop, which is more
machinery for the same result at this scale.

### Protocol errors close the connection

A malformed frame desynchronises the stream: there is no way to know where the
next command starts. So a protocol error gets `-ERR Protocol error: ...` and a
hang-up, while a *command* error (unknown name, wrong arity) is just a reply and
the connection continues. Redis draws the line in the same place, and the
distinction is the difference between "you said something I don't understand"
and "I no longer know where your sentences begin."

## Phase 7 — Benchmark

YCSB-style load generator (`src/bin/ycsb.rs`) speaking RESP, so the same binary
points at lsmrs or at a real `redis-server`. Zipfian key distribution
(theta 0.99, scrambled through `hash64`), 20k records, 40k operations,
8 client threads, 16-core machine.

Workload A is 50% read / 50% update; workload C is read-only.

All six configurations re-run back to back for consistency. Redis 7.0.15,
i7-13620H (16 threads), Linux 6.17, rustc 1.97.

| server | durability | workload | ops/sec | read p50 | read p99 | read p99.9 |
|---|---|---|---|---|---|---|
| lsmrs | fsync per write | A | 4,194 | 1967 us | 8057 us | 13124 us |
| redis | `appendfsync always` | A | 8,889 | 903 us | 2058 us | 5150 us |
| lsmrs | none | A | 187,362 | 29 us | 105 us | 183 us |
| redis | no AOF | A | 124,507 | 60 us | 111 us | 170 us |
| lsmrs | none | C | 270,485 | 21 us | 73 us | 138 us |
| redis | no AOF | C | 130,696 | 58 us | 97 us | 136 us |

### fsync is the whole story, 45x of it

Turning off WAL fsync takes lsmrs from 4,194 to 187,362 ops/sec. Nothing else
measured in this project comes close to that factor -- bloom filters were 20-52x
on a miss path that was itself microseconds. A single disk round-trip per write
dwarfs every in-process optimisation.

It is also why the earlier phase benchmarks used `sync: false`: with fsync on,
they would have measured the disk and nothing else.

### Redis is 2.1x faster at equal durability, and the reason is a real gap

`appendfsync always` is Redis's comparable setting, and it beats us 8,889 to
4,194. Redis is *single-threaded* and still wins, so this is not about cores.

The difference is **group commit**. Redis appends every command to its AOF
buffer and fsyncs once per event-loop iteration, so eight clients writing
concurrently cost one fsync between them. lsmrs fsyncs inside `put`, while
holding the global write lock, so eight concurrent writers produce eight
serialised fsyncs. Our durable write path is as slow as the disk *times the
number of writers*; Redis's is the disk *divided among* them.

This is the clearest concrete thing left undone in the project. Fixing it means
a commit queue: writers append to a shared buffer, one designated writer fsyncs
the batch, everyone waiting on that batch is woken. It is the standard design
and it is what every real WAL does.

### Where lsmrs wins, and why it doesn't count for much

Without fsync we beat Redis 1.5x on the mixed workload and 2.1x read-only. That
is 8 client threads on 16 cores against Redis's single thread. Per core, Redis
is several times ahead. A thread-per-connection server outrunning a
single-threaded one on a 16-core box is arithmetic, not engineering.

### Tail latency: a claim that did not survive a careful re-run

An early single run showed Redis at 178us p99.9 against our 358us without
fsync, and the conclusion drawn was that Redis has structurally tighter tails
because it has no lock contention and no compactor.

Re-running all six configurations back to back does not support that. Without
fsync the tails are close -- 183us against 170us at p99.9, and at p99 lsmrs is
actually *ahead* (105us vs 111us). The original 358us was run-to-run noise
being read as a finding.

Where Redis's tail advantage is real is *with* fsync: 5150us against our
13124us at p99.9. That is the same group-commit story as the throughput gap.
Under group commit a writer waits for at most one batch; under our
fsync-per-write-under-a-global-lock, a writer waits for every writer ahead of
it, so the worst case grows with concurrency.

The lesson is about method, not about Redis: one run of a latency benchmark is
an anecdote, and the tail is exactly where that bites. Throughput numbers were
stable to within a few percent across runs; p99.9 moved by 2x.

### What the read-only number says

270k ops/sec on workload C, against 187k on A, isolates the write lock:
removing writes entirely is worth 44%. That gap is the cost of `RwLock<Db>` -- readers
queueing behind exclusive writers -- and it is the second thing worth fixing
after group commit.
