use lsmrs::{Db, config::Config};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("lsmrs_it_{}_{}", std::process::id(), n))
}

fn config_at(path: PathBuf, table_size: usize) -> Config {
    Config {
        path,
        sync: false,
        table_size,
        ..Config::default()
    }
}

#[test]
fn put_get_delete_roundtrip() {
    let mut db = Db::with_config(config_at(temp_path(), usize::MAX)).unwrap();
    db.put(b"alpha".to_vec(), b"1".to_vec()).unwrap();
    db.put(b"beta".to_vec(), b"2".to_vec()).unwrap();

    assert_eq!(db.get(b"alpha").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"beta").unwrap(), Some(b"2".to_vec()));

    db.delete(b"alpha").unwrap();
    assert_eq!(db.get(b"alpha").unwrap(), None);
    assert_eq!(db.get(b"beta").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn data_survives_reopen_across_a_flush() {
    let path = temp_path();
    {
        // A threshold of 0 forces a flush on every put after the first.
        let mut db = Db::with_config(config_at(path.clone(), 0)).unwrap();
        db.put(b"alpha".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"beta".to_vec(), b"2".to_vec()).unwrap();
        db.put(b"gamma".to_vec(), b"3".to_vec()).unwrap();
    }

    let db = Db::with_config(config_at(path, 0)).unwrap();
    assert_eq!(db.get(b"alpha").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get(b"beta").unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get(b"gamma").unwrap(), Some(b"3".to_vec()));
}
