# lsmrs

A learning project: an LSM-tree key-value store built from scratch in Rust —
WAL, SSTables, bloom filters, and compaction. No storage-engine crates, and
**0 LOC of code written by coding agents** — every line is hand-written.

The goal isn't to ship a database; it's to understand how one works by building
the layers a real LSM engine (LevelDB, RocksDB) is made of, one at a time.
Architecture follows *Designing Data-Intensive Applications*, 2nd ed., Ch. 4
(Storage and Retrieval).

> **Status:** Phase 2 of 7 complete. The in-memory store, CLI, and the
> write-ahead log (with crash recovery) work today. SSTables, bloom filters,
> compaction, and the network layer are still ahead — see [Build Phases](#build-phases).

---

## Quick start

```bash
cargo run            # start the REPL
```

```text
lsmrs> PUT foo bar
OK
lsmrs> GET foo
bar
lsmrs> SCAN
foo bar
lsmrs> DEL foo
OK
lsmrs> GET foo
(not found)
lsmrs> EXIT
```

### Commands

| Command          | Description                          |
|------------------|--------------------------------------|
| `PUT <key> <val>`| Insert or overwrite a key            |
| `GET <key>`      | Look up a key                        |
| `DEL <key>`      | Delete a key                         |
| `SCAN`           | Print all key/value pairs, in order  |
| `EXIT`           | Quit                                 |

Writes are durably appended to a write-ahead log before they touch memory, so
data survives a restart — quit, relaunch, and your keys are still there.

---

## Design

### The write path

```
PUT k v
   │
   ▼
  WAL  ──(append record, optional fsync)──▶  disk
   │   write succeeds?
   ▼
memtable (BTreeMap)  ──▶  in-memory, sorted by key
```

Every mutation is appended to the **write-ahead log first**, and only applied to
the in-memory **memtable** if the log write succeeds. The WAL is the source of
truth; the memtable is a derived, in-memory view. On startup the log is replayed
to rebuild the memtable exactly as it was before shutdown.

A `BTreeMap` (not a hash map) backs the memtable because the LSM design needs
**ordered** keys — for range scans now, and for merge-sorted SSTable flushes
later. Keys and values are raw `Vec<u8>`, so arbitrary binary data works.

### WAL record format

Each record is a length-prefixed, CRC-checked binary frame:

```
[length: u32][crc32: u32][key_len: u32][key][value_len: u32][value][op_type: u8]
 └─ 4 ──────┘└─ 4 ──────┘└──────────────────── payload ──────────────────────┘
```

- All integers are **little-endian**.
- `op_type`: `0` = Insert, `1` = Delete (a delete is an appended tombstone, not
  an in-place edit — the log is append-only).
- `length` covers the checksum field plus the payload (not its own 4 bytes).
- A **binary** format is required, not text: keys and values can contain spaces,
  newlines, or non-UTF-8 bytes, so explicit length prefixes are the only
  unambiguous way to delimit them.

### Crash recovery: truncation vs. corruption

Replay treats the two ways a log tail can fail very differently:

| On disk                          | Cause                        | Replay does            |
|----------------------------------|------------------------------|------------------------|
| File ends mid-record (`EOF`)     | Crash during append          | **Recover** — stop, keep prior records |
| Full record, CRC mismatch        | Corruption of complete data  | **Error** — `InvalidData` |

A half-written trailing record from a crash never completed, so discarding it is
correct. A *complete* record whose checksum fails is an integrity violation and
must not be silently accepted. Both behaviors are pinned by unit tests.

### Durability is configurable

`write_all` only hands bytes to the OS page cache; a crash before the OS flushes
still loses them. `fsync` forces bytes to the physical disk but is slow. The
trade-off is a per-database choice (`Config { sync }`):

- `sync = true` — every write durable, slower.
- `sync = false` — fast, last few writes lost on crash (tests, throwaway data).

For deeper notes and per-phase rationale, see [`DESIGN.md`](DESIGN.md).

---

## Build phases

The store is built incrementally; each phase is fully working and tested before
the next begins.

- [x] **Phase 1 — In-memory KV + CLI:** `BTreeMap` store, `rustyline` REPL,
      `thiserror` errors.
- [x] **Phase 2 — Write-ahead log:** binary record format, CRC validation,
      replay on startup, configurable `fsync`, torn-write recovery.
- [ ] **Phase 3 — Memtable → SSTable flush:** sorted data blocks, sparse index,
      footer; layered reads; WAL truncation after flush.
- [ ] **Phase 4 — Bloom filters + block index:** per-SSTable bloom filter,
      binary search on the sparse index, before/after benchmarks.
- [ ] **Phase 5 — Compaction:** background size-tiered merge, tombstone cleanup,
      `Arc<RwLock>` coordination, no stop-the-world.
- [ ] **Phase 6 — Network protocol:** TCP server speaking a Redis RESP subset,
      usable from `redis-cli`.
- [ ] **Phase 7 — Polish & benchmark:** YCSB-style workload, latency histograms
      (p50/p95/p99), comparison notes.

---

## Project layout

```
src/
├── main.rs      # CLI binary, rustyline REPL
├── lib.rs       # re-exports
├── db.rs        # Db: memtable + WAL coordination, get/put/delete/scan
├── wal.rs       # write-ahead log: record format, append, replay
├── config.rs    # Config (path, sync)
├── error.rs     # DbError
└── cli/
    └── mod.rs   # command parsing + execution
tests/
└── db_test.rs   # integration tests against the public API
```

---

## Development

```bash
cargo build                     # build
cargo test                      # run unit + integration tests
cargo clippy -- -W clippy::all  # lint (must be warning-free)
cargo fmt                       # format
```

All code passes `clippy` with no warnings and is `rustfmt`-formatted. Unit tests
live alongside the code they test; integration tests live in `tests/`.

---

## Non-goals

This is a study project, optimized for **clarity over cleverness**. It does not
use any crate that implements the storage engine itself — bloom filters,
SSTables, compaction, and the WAL are all written by hand. It is not intended for
production use.
