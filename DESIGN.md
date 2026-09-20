# Design Notes

Architecture decisions for lsmrs, recorded as each phase lands. The guiding
principle (see CLAUDE.md) is *obvious over clever* — these notes explain the
*why* behind choices that aren't self-evident from the code.

---

## Phase 1 — In-Memory KV + CLI

- **`BTreeMap` over `HashMap`** for the memtable. The LSM read path needs
  *ordered* iteration for range scans and, later, for merge-sorted SSTable
  flushes. A hash map would force a sort on every scan/flush.
- Keys and values are `Vec<u8>`, not `String` — the store is byte-oriented, so
  arbitrary binary keys/values must work (a key may contain spaces, newlines,
  or non-UTF-8 bytes).

---

## Phase 2 — Write-Ahead Log (WAL)

### Why a WAL

The memtable lives in RAM and is lost on crash. The WAL is the durable record
of every mutation. Writes go to the WAL *before* the memtable, so on restart we
can replay the log and reconstruct the exact pre-crash memtable state.

### Record format

```
[length: u32][crc32: u32][key_len: u32][key][value_len: u32][value][op_type: u8]
└── 4 ──────┘└── 4 ─────┘└─────────────────── payload ───────────────────────┘
```

- All integers are **little-endian** (matches LevelDB/RocksDB convention and
  the native byte order of the target hardware).
- `length` = `4 (checksum) + payload.len()`. It counts the checksum field plus
  the payload, but **not** its own 4 bytes. On read we consume `length` bytes
  after the length field: 4 for the checksum, the rest is payload.
- A **binary** format (not text like `SET k v`) is required because keys and
  values are arbitrary bytes — a text format can't unambiguously delimit a
  value containing a space or newline. Explicit length prefixes remove all
  ambiguity.

### Truncation vs corruption — two distinct tail failures

Replay distinguishes two things that can go wrong at the end of the log:

| On disk | Cause | Replay behavior |
|---|---|---|
| File ends mid-record (`UnexpectedEof`) | Crash during append — partial write | **Recover**: stop, keep records so far |
| Full record, CRC mismatch | Corruption of complete data | **Error**: `InvalidData` |

This is deliberate. A half-written trailing record from a crash is *expected*
and safely discarded — the operation never completed, so losing it is correct.
A complete record whose checksum fails is an *integrity violation* we must not
silently accept. Both cases are pinned by unit tests.

### Write-before-memtable ordering

`put`/`delete` call `wal.{insert,delete}()?` first; only on `Ok` do they touch
the memtable. If the WAL write fails, the memtable is left untouched and the
error propagates. This keeps the log and the in-memory state from ever
diverging — the WAL is the source of truth, the memtable a derived view.

### Configurable `fsync`

`write_all` only hands bytes to the OS page cache; a crash before the OS
flushes still loses data. `sync_data()` (fsync) forces bytes to physical disk
but is slow (waits on the device). So durability is a per-`Db` choice via
`Config { sync }`:

- `sync = true` — every write durable, slower (production default).
- `sync = false` — fast, last few writes lost on crash (tests, throwaway data).

### Deferred

- *Truncate WAL after SSTable flush* — depends on SSTables; lands in Phase 3.

---

## Phase 3 — Memtable → SSTable Flush

- **Immutable files.** An SSTable is written once and never modified. Every
  later design choice depends on this: readers need no locks against writers,
  compaction can read inputs while serving traffic, and a merge can publish its
  output before deleting what it consumed.
- **Sparse index, not a full one.** One index entry per 4KB block rather than
  per key. A lookup binary-searches the index for the block a key *would* be in,
  then scans forward inside it. Trades a bounded scan for an index small enough
  to keep in memory.
- **Tombstones are records.** A delete writes a record with `op_type = 1` and an
  empty value. It cannot be inferred from an empty value, because an empty
  *value* is legal — one byte separates "deleted" from "set to nothing".
- **Read order is newest-first** across tables, stopping at the first answer. A
  tombstone in a newer table is an answer: it means "absent", not "keep looking".

## Phase 4 — Bloom Filters

- **The filter is per-table and lives in the file.** `m` and `k` are serialized
  alongside the bits rather than recomputed from `Config`, so changing the
  config knob later cannot silently mis-read files already on disk. Config is
  write-time policy; the parameters are properties of the bytes.
- **Sized from the exact key count.** At flush time `memtable.len()` is known,
  so `m = n * bits_per_key` and `k = round(ln2 * bits_per_key)` are exact rather
  than estimated. At 10 bits/key the measured false-positive rate is 0.85%
  against a theoretical 0.82%.
- **Tombstones go in the filter.** Omitting them would make a newer table's
  delete invisible, and the read would fall through to an older table and
  resurrect the value.
- **A hand-written, pinned hash.** `std`'s `DefaultHasher` is explicitly allowed
  to change between Rust releases. For anything whose output is persisted, that
  is silent data loss: filters written by one build, misread by the next. So
  FNV-1a plus a MurmurHash3 finalizer, with golden-value tests that fail loudly
  if the algorithm ever moves.
- **One hash per lookup, not `k`.** Kirsch–Mitzenmacher double hashing: split a
  single 64-bit hash into a start and a stride, then walk. And the hash is
  computed once per `get` in `SSTableSet`, not once per table.

## Phase 5 — Compaction

- **The lock protects the list, not the files.** SSTables are immutable, so
  concurrent readers of one file need no coordination at all. The only shared
  mutable state is *which files exist*. The merge and the output write happen
  unlocked; the write lock is held for a pointer swap.
- **`Arc<RwLock<Arc<Vec<Arc<SSTable>>>>>`.** A reader clones the inner `Arc` —
  one atomic increment, no allocation — and does all I/O unlocked. The `Vec` is
  immutable once published; a writer builds a new one and swaps. Putting the
  `Vec` directly under the lock instead cost a heap allocation per read and
  measurably regressed the miss path.
- **Descriptors held open; deletion needs no coordination.** `unlink` removes a
  name and the inode survives until the last descriptor closes, so a compactor
  may delete a file a reader is mid-scan on. This requires `read_exact_at`
  (`pread`), which takes `&self` — `Read + Seek` would have forced a
  `Mutex<File>` and serialised every reader.
- **Filenames record spans.** `sstable-{first}-{last}.sst` names the range of
  flushes a table contains. Sorting by the first number keeps a merge in its
  inputs' position rather than letting it sort last and shadow newer data, and
  any span contained in another span is a crash leftover. No MANIFEST needed.
- **Atomic publish.** Write to `.tmp`, fsync, `rename`, fsync the directory. A
  file under its final name is complete by construction — which is exactly what
  makes the span-containment recovery rule sound.
- **Tombstones only drop in the oldest merge.** Anywhere else, an older table
  may still hold the value the tombstone is hiding.
- **Shutdown is the absence of senders.** `recv()` returns `Err` when every
  `Sender` has dropped, so `Db::drop` takes the sender and joins. No stop flag.
  A side effect is that background compaction becomes deterministic to test.

## Phase 6 — Network Protocol

- **RESP2 subset over thread-per-connection.** `Db::get` already took `&self`
  and `put`/`delete` `&mut self`, so `Arc<RwLock<Db>>` maps onto the existing
  API unchanged: reads concurrent, writes exclusive.
- **Length prefixes, because the data is arbitrary bytes.** The same reasoning
  as the SSTable and WAL record formats. A `SET` whose value contains `\r\n` —
  the protocol's own terminator — round-trips intact.
- **A length prefix from a socket is an allocation instruction from a
  stranger.** `MAX_BULK_LEN` is checked before the `vec![0u8; len]`, not after.
  This is the first untrusted input in the project.
- **Command errors reply; protocol errors hang up.** An unknown command or bad
  arity is a normal reply and the connection continues. A malformed frame
  desynchronises the stream — there is no way to know where the next command
  begins — so it gets `-ERR Protocol error` and a close.
- **Shutdown wakes a blocked `accept` by connecting to itself.** A flag alone
  cannot interrupt `accept`; the self-connection is what lets the loop notice.
  It also makes the server joinable in tests rather than leaking a thread each.

## Phase 7 — Benchmark

Measured with a YCSB-style generator speaking RESP, so the identical binary
drives both lsmrs and a real `redis-server`. Full tables in NOTES.md.

- **fsync dominates everything, by 45x.** 4,194 ops/sec with WAL fsync per
  write, 187,362 without. No in-process optimisation in this project comes
  within an order of magnitude of that factor.
- **Redis is 2.1x faster at equal durability, and the reason is group commit.**
  Redis fsyncs its AOF once per event-loop iteration, so N concurrent writers
  share one disk round-trip. lsmrs fsyncs inside `put` while holding the global
  write lock, so N writers cost N serialised fsyncs. This is the single largest
  piece of known work left.
- **Where lsmrs is faster, it is arithmetic.** 1.5–2.1x ahead without fsync, on
  8 threads against Redis's one, on a 16-core box. Per core Redis is well ahead.
- **Tails are close without fsync and far apart with it.** 183µs vs 170µs at
  p99.9 unsynced; 13124µs vs 5150µs synced. Group commit again: it bounds how
  long a writer can be stuck behind other writers, where fsync-under-a-global-
  lock does not.
- **Read-only is 44% faster than the mixed workload**, which is the cost of
  `RwLock<Db>` serialising writers ahead of readers — the second thing to fix
  after group commit.
