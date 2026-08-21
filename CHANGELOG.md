# Changelog

All notable changes to lsmrs are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

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
