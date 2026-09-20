# lsmrs

A learning project: an LSM-tree key-value store built from scratch in Rust —
WAL, SSTables, bloom filters, and compaction. No storage-engine crates used.

The goal isn't to ship a database; it's to understand how one works by building
the layers a real LSM engine (LevelDB, RocksDB) is made of, one at a time.
Architecture follows *Designing Data-Intensive Applications*, 2nd ed., Ch. 4
(Storage and Retrieval).

> **Status:** all 7 phases complete. WAL with crash recovery, SSTables with
> sparse indexes and bloom filters, background size-tiered compaction, and a
> Redis-protocol server you can drive with `redis-cli`.

---

## Benchmarks

20k records, 40k operations, 8 client threads, Zipfian keys (theta 0.99).
lsmrs against Redis 7.0.15 on the same machine — i7-13620H, 16 threads,
Linux 6.17, rustc 1.97. Workload A is 50% read / 50% update; C is read-only.

| Workload | Durability | Server | ops/sec | read p50 | read p99 | read p99.9 |
|---|---|---|---:|---:|---:|---:|
| A | fsync per write | lsmrs | 4,194 | 1967 µs | 8057 µs | 13124 µs |
| A | fsync per write | Redis | **8,889** | 903 µs | 2058 µs | 5150 µs |
| A | none | lsmrs | **187,362** | 29 µs | 105 µs | 183 µs |
| A | none | Redis | 124,507 | 60 µs | 111 µs | 170 µs |
| C | none | lsmrs | **270,485** | 21 µs | 73 µs | 138 µs |
| C | none | Redis | 130,696 | 58 µs | 97 µs | 136 µs |

**fsync costs 45×.** 4,194 ops/sec with WAL fsync per write against 187,362
without. Nothing else in this project comes within an order of magnitude of
that factor — bloom filters, the largest in-process win, were 20–52× on a path
already measured in microseconds.

**Redis is 2.1× faster at equal durability, and the reason is specific.** It is
single-threaded and still wins, so this is not about cores: Redis *group
commits*, fsyncing its AOF once per event-loop iteration so N concurrent
writers share one disk round-trip. lsmrs fsyncs inside `put` while holding the
global write lock, so N writers cost N serialised fsyncs. That is the single
largest piece of known work left.

**Where lsmrs is ahead, it is arithmetic.** 1.5× on mixed and 2.1× read-only,
running 8 client threads on 16 cores against Redis's one. Per core Redis is
several times further ahead. Tail latencies land close when neither is fsyncing
(183 µs vs 170 µs at p99.9) and Redis pulls clearly ahead when both are
(5150 µs vs 13124 µs) — again group commit, which bounds how long a writer can
be stuck behind others.

Reproduce:

```bash
cargo run --release -- serve 127.0.0.1:7379 --no-sync
cargo run --release --bin ycsb -- --addr 127.0.0.1:7379 --workload a
```

`ycsb` takes `--workload a|b|c`, `--records`, `--ops`, `--threads`, and
`--distribution zipfian|uniform`, and speaks RESP — so it drives a real
`redis-server` unchanged.

### Bloom filters

Phase 4, measured against the phase-3 read path (10k keys, ~13 SSTables):

| operation | before | after |
|---|---:|---:|
| miss, overlapping key ranges | 63.9 µs | 1.22 µs |
| miss, disjoint ranges | 5.13 µs | 250 ns |

---

## Quick start

```bash
cargo run                       # REPL
cargo run --release -- serve    # Redis-protocol server on 127.0.0.1:6379
```

```bash
$ redis-cli -p 6379 SET greeting "hello from lsmrs"
OK
$ redis-cli -p 6379 GET greeting
"hello from lsmrs"
$ redis-cli -p 6379 DEL greeting
(integer) 1
```

Supported commands: `PING`, `ECHO`, `GET`, `SET`, `DEL`, `QUIT`.

### REPL

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
- [x] **Phase 3 — Memtable → SSTable flush:** sorted data blocks, sparse index,
      footer; layered reads; WAL truncation after flush.
- [x] **Phase 4 — Bloom filters + block index:** per-SSTable bloom filter,
      binary search on the sparse index, before/after benchmarks.
- [x] **Phase 5 — Compaction:** background size-tiered merge, tombstone cleanup,
      `Arc<RwLock>` coordination, no stop-the-world.
- [x] **Phase 6 — Network protocol:** TCP server speaking a Redis RESP subset,
      usable from `redis-cli`.
- [x] **Phase 7 — Polish & benchmark:** YCSB-style workload, latency histograms
      (p50/p95/p99), comparison notes.

---

## Project layout

```
src/
├── main.rs        # REPL and `serve` entry point
├── lib.rs         # re-exports
├── db.rs          # Db: memtable, WAL, flush, compactor thread
├── wal.rs         # write-ahead log: record format, append, replay
├── sstable.rs     # SSTable format, read path, merge, compaction
├── bloom.rs       # bit array, probe walk
├── hash.rs        # FNV-1a + MurmurHash3 finalizer, pinned by golden tests
├── resp.rs        # RESP2 parsing and reply encoding
├── server.rs      # TCP listener, command dispatch, shutdown
├── config.rs      # Config
├── error.rs       # DbError
├── cli/mod.rs     # REPL command parsing
└── bin/ycsb.rs    # YCSB-style load generator
tests/
├── db_test.rs     # integration tests against the public API
└── server_test.rs # integration tests over a real socket
benches/
└── read_path.rs   # criterion benchmarks
```

For the reasoning behind each phase see [`DESIGN.md`](DESIGN.md); for the
things learned along the way, [`NOTES.md`](NOTES.md).

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
