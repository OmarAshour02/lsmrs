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
