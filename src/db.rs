use crate::wal::{Operation, Wal};
use std::collections::BTreeMap;
use std::io;

pub struct Db {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    wal: Wal,
}

impl Db {
    pub fn open(path: &str, sync: bool) -> Result<Self, io::Error> {
        let mut wal = Wal::open(path, sync)?;
        let records = wal.read()?;
        let mut map = BTreeMap::new();
        for r in records {
            match r.operation {
                Operation::Insert => {
                    map.insert(r.key, r.value);
                }
                Operation::Delete => {
                    map.remove(&r.key);
                }
            }
        }
        Ok(Self { map, wal })
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).cloned()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, io::Error> {
        self.wal.insert(&key, &value)?;
        Ok(self.map.insert(key, value))
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
        self.wal.delete(key)?;
        Ok(self.map.remove(key))
    }

    pub fn scan(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.map.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SYNC: bool = false;
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("lsmrs_db_test_{}_{}", std::process::id(), n))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn put_then_get_returns_value() {
        let mut db = Db::open(&temp_path(), SYNC).unwrap();
        let _x = db.put(b"foo".to_vec(), b"bar".to_vec());
        assert_eq!(db.get(b"foo"), Some(b"bar".to_vec()));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = Db::open(&temp_path(), SYNC).unwrap();
        assert_eq!(db.get(b"missing"), None);
    }

    #[test]
    fn delete_removes_key() {
        let mut db = Db::open(&temp_path(), SYNC).unwrap();
        let _x = db.put(b"k".to_vec(), b"v".to_vec());
        assert!(db.delete(b"k").unwrap().is_some());
        assert_eq!(db.get(b"k"), None);
    }

    #[test]
    fn delete_missing_returns_none() {
        let mut db = Db::open(&temp_path(), SYNC).unwrap();
        assert!(db.delete(b"nope").unwrap().is_none());
    }

    #[test]
    fn put_overwrites_existing_value() {
        let mut db = Db::open(&temp_path(), SYNC).unwrap();
        let _x = db.put(b"k".to_vec(), b"v1".to_vec());
        let _y = db.put(b"k".to_vec(), b"v2".to_vec());
        assert_eq!(db.get(b"k"), Some(b"v2".to_vec()));
    }
}
