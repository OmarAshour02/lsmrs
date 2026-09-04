# Changelog

All notable changes to lsmrs are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

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
