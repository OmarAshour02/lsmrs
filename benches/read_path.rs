use criterion::{Criterion, criterion_group, criterion_main};
use lsmrs::{Db, config::Config};
use std::hint::black_box;
use std::path::{Path, PathBuf};

const KEY_COUNT: usize = 10_000;
// Small enough that KEY_COUNT records spill into roughly a dozen SSTables,
// so a lookup has several files to consider.
const TABLE_SIZE: usize = 64 * 1024;

fn key(i: usize) -> Vec<u8> {
    format!("key{:08}", i).into_bytes()
}

fn value(i: usize) -> Vec<u8> {
    format!("{:0>64}", i).into_bytes()
}

fn config(path: PathBuf) -> Config {
    Config {
        path,
        sync: false,
        table_size: TABLE_SIZE,
        bits_per_key: 10,
    }
}

// Populates a fresh directory, then reopens so the memtable starts empty and
// every read has to go to disk. Only even keys are stored, so the odd ones make
// misses that fall *inside* every table's key range rather than being rejected
// by the min/max check.
fn populated_db(dir: &Path) -> Db {
    let mut db = Db::with_config(config(dir.to_path_buf())).unwrap();
    for i in (0..KEY_COUNT * 2).step_by(2) {
        db.put(key(i), value(i)).unwrap();
    }
    drop(db);
    Db::with_config(config(dir.to_path_buf())).unwrap()
}

fn bench_reads(c: &mut Criterion) {
    let dir = std::env::temp_dir().join(format!("lsmrs_bench_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let db = populated_db(&dir);

    let mut group = c.benchmark_group("read_path");

    // Odd keys were never written, so every SSTable has to be opened and
    // scanned before the miss is confirmed. This is the case bloom filters
    // target in Phase 4.
    group.bench_function("get_miss", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % KEY_COUNT;
            black_box(db.get(black_box(&key(i * 2 + 1))).unwrap())
        })
    });

    group.bench_function("get_hit", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % KEY_COUNT;
            black_box(db.get(black_box(&key(i * 2))).unwrap())
        })
    });

    group.finish();
    std::fs::remove_dir_all(&dir).ok();
}

// Sequential inserts give each SSTable a contiguous, disjoint key range, so the
// min/max check alone narrows any lookup to one table. Scattering the insert
// order makes the ranges overlap, which is the realistic case -- and the one
// where a per-table bloom filter actually has work to do.
fn scattered(i: usize) -> usize {
    (i * 7919) % KEY_COUNT * 2
}

fn bench_overlapping_reads(c: &mut Criterion) {
    let dir = std::env::temp_dir().join(format!("lsmrs_bench_o_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();

    let mut db = Db::with_config(config(dir.clone())).unwrap();
    for i in 0..KEY_COUNT {
        let k = scattered(i);
        db.put(key(k), value(k)).unwrap();
    }
    drop(db);
    let db = Db::with_config(config(dir.clone())).unwrap();

    let mut group = c.benchmark_group("read_path_overlapping");

    group.bench_function("get_miss", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % KEY_COUNT;
            black_box(db.get(black_box(&key(i * 2 + 1))).unwrap())
        })
    });

    group.bench_function("get_hit", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % KEY_COUNT;
            black_box(db.get(black_box(&key(scattered(i)))).unwrap())
        })
    });

    group.finish();
    std::fs::remove_dir_all(&dir).ok();
}

fn bench_writes(c: &mut Criterion) {
    c.bench_function("put", |b| {
        let dir = std::env::temp_dir().join(format!("lsmrs_bench_w_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut db = Db::with_config(config(dir.clone())).unwrap();
        let mut i = 0;
        b.iter(|| {
            i += 1;
            db.put(black_box(key(i)), black_box(value(i))).unwrap()
        });
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    });
}

criterion_group!(benches, bench_reads, bench_overlapping_reads, bench_writes);
criterion_main!(benches);
