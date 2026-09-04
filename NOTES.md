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
