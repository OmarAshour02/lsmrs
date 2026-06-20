# Changelog

All notable changes to lsmrs are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

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
