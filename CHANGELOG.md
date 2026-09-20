# Changelog

All notable changes to lsmrs are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Phase 7 — Benchmark

- `src/bin/ycsb.rs`: a YCSB-style load generator speaking RESP, so the same
  binary drives lsmrs or a real `redis-server`. Workloads A (50/50), B (95/5)
  and C (read-only), uniform or Zipfian keys, per-operation latency percentiles.
- `lsmrs serve --no-sync` runs without WAL fsync, for benchmarking against
  servers at matching durability.
- Headline numbers (20k records, 40k ops, 8 threads, Zipfian): fsync per write
  costs 43x — 4,011 ops/sec with it, 172,971 without. Redis at
  `appendfsync always` is 2.3x faster than lsmrs at equal durability because it
  group-commits its AOF; lsmrs fsyncs per write under the global lock.
- Full analysis and the comparison matrix in NOTES.md; per-phase rationale for
  Phases 3-7 added to DESIGN.md.

### Phase 6 — Network Protocol

- `lsmrs serve [addr]` starts a TCP server speaking a RESP2 subset, so
  `redis-cli` and other Redis clients work against it unmodified. Default
  address is `127.0.0.1:6379`.
- Commands: `PING`, `ECHO`, `GET`, `SET`, `DEL`, `QUIT`, and `COMMAND` (which
  `redis-cli` sends on connect).
- `src/resp.rs` parses both RESP arrays and inline commands, and rejects a
  bulk-string length over 64MB *before* allocating — the first untrusted input
  in the project.
- Thread per connection over `Arc<RwLock<Db>>`: reads run concurrently, writes
  take the exclusive lock. `try_clone` splits the socket so the reader and
  writer halves can be owned independently.
- Command errors (unknown name, wrong arity) reply and keep the connection;
  protocol errors reply and hang up, because a desynchronised stream cannot be
  resynchronised.
- Graceful shutdown: `ShutdownHandle::shutdown` sets a flag and connects to the
  listener's own port to wake the blocked `accept`, which also makes the server
  joinable in tests.

### Phase 5 — Compaction

- Background compactor thread, woken by an `mpsc` channel after each flush and
  stopped by the `Sender` dropping. `Db::drop` joins it, which also makes the
  thread deterministic to test.
- Size-tiered selection: `pick_run` groups tables whose sizes are within
  `Config { compaction_size_ratio }` and merges once
  `Config { compaction_threshold }` of them accumulate (defaults 1.5 and 4), so
  a small flush is never rewritten into a large table.
- Reads are never blocked by a merge. `SSTableSet` holds
  `Arc<RwLock<Arc<Vec<Arc<SSTable>>>>>`; a reader clones one `Arc` under the
  read lock and does all I/O unlocked, while the compactor merges outside the
  lock and takes the write lock only to swap the pointer.
- `SSTable` keeps its `File` open and reads through `read_exact_at` (`pread`),
  which takes `&self` — so many threads share one descriptor with no `Mutex`.
  Deleting a compacted file under a live reader is safe: `unlink` drops the
  name, the inode outlives it until the last descriptor closes.
- SSTables are named `sstable-{first}-{last}.sst`, recording the span of
  flushes they contain. Sorting by the first number keeps a merge in its
  inputs' position instead of letting it shadow newer data, and any file whose
  span is contained in another's is a crash leftover, deleted on open. No
  MANIFEST needed.
- Every table is now published atomically: written to `.tmp`, fsynced, renamed,
  and the directory fsynced. A file under its final name is always complete.
- Tombstones are dropped only when the merge includes the oldest live table;
  anywhere else they are carried forward, or an older value resurfaces.
- Reads after compaction: overlapping misses 1.22µs → 548ns, hits 7.13µs →
  4.94µs. Puts 3.14µs → 3.96µs, all of it the directory fsync.

### Phase 4 — Bloom Filters

- Per-SSTable bloom filter, built at flush and loaded into memory on open.
  Sized from the exact key count: `m = n * bits_per_key`,
  `k = round(ln2 * bits_per_key)`, with `Config { bits_per_key }` defaulting
  to 10 (measured 0.85% false positives).
- `m` and `k` are serialized with the filter rather than recomputed from
  config, so changing `bits_per_key` cannot invalidate existing files.
- Tombstones are inserted into the filter alongside live keys; skipping them
  would let a newer table's delete go unseen and resurrect an old value.
- New `hash::hash64` — FNV-1a plus a MurmurHash3 finalizer, pinned by
  golden-value tests. `std`'s `DefaultHasher` is unusable here because its
  algorithm may change between Rust releases.
- Probe positions use Kirsch-Mitzenmacher double hashing: one hash per lookup,
  split into a start and a stride, rather than `k` independent hashes. The
  hash is computed once in `SSTableSet::get` and reused across every table.
- SSTable format v1: `[data][index][filter][meta][trailer]`. The trailer grows
  to 32 bytes — three block offsets, a `LSMRS\0` magic, and a version byte, so
  a format change fails loudly instead of reading garbage as offsets.
- Reads: misses 20-52x faster, hits in overlapping tables 9.4x faster
  (see NOTES.md for the full table). Writes unchanged.

### Phase 3 — Memtable → SSTable Flush

- Memtable flushes to an immutable SSTable once `Config { table_size }` is
  exceeded; the WAL is truncated only after the SSTable is `fsync`ed.
- SSTable format (all integers little-endian):
  `[data blocks][sparse index][metadata][index_start: u64][meta_start: u64]`.
  Records are `[key_len: u32][key][value_len: u32][value][op_type: u8]`,
  index entries are `[key_len: u32][key][offset: u64]`, and metadata holds
  the length-prefixed `min_key` and `max_key`. The 16-byte trailer is the
  fixed-size anchor `SSTable::open` seeks to.
- One sparse index entry per 4KB block; lookups binary-search the index for
  the containing block, then scan records forward within it.
- Read path: memtable first (a tombstone there shadows every SSTable), then
  SSTables newest-to-oldest, stopping at the first table with an answer.
- Tombstones survive the flush and are honoured on read, so a delete is not
  resurrected by an older SSTable.
- Flush ordering fixed so a record is inserted into the memtable before the
  threshold check, closing a window where a logged write could be erased from
  the WAL while living only in RAM.
- `delete` now counts toward the flush threshold, so tombstone-heavy
  workloads still flush.
- `SSTableSet::write` takes `&mut self` and registers the new table, so
  flushed data is readable in-process and sequence numbers advance (previously
  every flush overwrote `sstable-000000.sst`).
- `Db::get` and `SSTableSet::get` return `Result`; `DbError` gained an `Io`
  variant so I/O failures are no longer reported as "key not found".
- `Config` gained `table_size` (default 4MB).
- Tests: byte-level trailer assertions, write/open roundtrip, multi-block
  lookups, misses inside and outside the key range, newest-table-wins
  ordering, tombstone shadowing, WAL replay and truncation across reopen.
- Criterion benchmarks (`benches/read_path.rs`) covering `get` hit/miss with
  disjoint and overlapping SSTable key ranges, plus `put`.

### Phase 2 — Write-Ahead Log (WAL)

- Append-only WAL with binary record format:
  `[length: u32][crc32: u32][key_len: u32][key][value_len: u32][value][op_type: u8]`
  (all integers little-endian; `op_type` 0 = Insert, 1 = Delete).
- CRC32 checksum per record, verified on read.
- WAL replay on startup (`Db::open`) rebuilds the memtable by re-applying
  every logged operation in order.
- Write-before-memtable ordering: `put`/`delete` append to the WAL first and
  only touch the memtable if the log write succeeds (errors propagate).
- Configurable `fsync` per write via `Config { sync }`.
- Torn-write recovery: a truncated trailing record (crash mid-append) is
  discarded on replay; a complete-but-corrupt record (CRC mismatch) is an error.
- `Config` struct (`config.rs`) holding `path` and `sync`, with `Default`.
- Unit tests: write/read roundtrip, CRC validation, torn-write recovery.
  Each test uses an isolated temp WAL file.

### Phase 1 — In-Memory KV + CLI

- `Db` wrapping `BTreeMap<Vec<u8>, Vec<u8>>` with `get` / `put` / `delete` / `scan`.
- `rustyline` REPL (`lsmrs>` prompt) with `GET`, `PUT`, `DEL`, `SCAN`, `EXIT`.
- `DbError` error type via `thiserror`.
