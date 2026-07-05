use crate::config::Config;
use crate::sstable::{SSTable, SSTableSet};
use crate::wal::{Operation, Wal};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub struct Db {
    map: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    wal: Wal,
    size: usize,
    config: Config,
    sstable_set: SSTableSet,
}

impl Db {
    pub fn open() -> Result<Self, io::Error> {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Result<Self, io::Error> {
        let mut wal = Wal::open(&config.path, config.sync)?;
        let records = wal.read()?;
        let mut map = BTreeMap::new();
        for r in records {
            match r.operation {
                Operation::Insert => {
                    map.insert(r.key, Some(r.value));
                }
                Operation::Delete => {
                    map.insert(r.key, None);
                }
            }
        }

        let sstable_set = SSTableSet::open(PathBuf::from(&config.path))?;

        Ok(Self {
            map,
            wal,
            size: 0,
            config,
            sstable_set,
        })
    }

    fn flush(&mut self) -> Result<SSTable, io::Error> {
        self.sstable_set.write(&self.map)
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).cloned().flatten()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, io::Error> {
        self.wal.insert(&key, &value)?;
        self.size += key.len() + value.len();
        if self.size > self.config.table_size && self.map.len() > 1 {
            self.flush()?;
            self.size = 0;
        }
        Ok(self.map.insert(key, Some(value)).flatten())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
        self.wal.delete(key)?;
        Ok(self.map.insert(key.to_vec(), None).flatten())
    }

    pub fn scan(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.map
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| (k, v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("lsmrs_db_test_{}_{}", std::process::id(), n))
            .to_string_lossy()
            .into_owned()
    }

    fn test_config() -> Config {
        Config {
            path: temp_path(),
            sync: false,
            ..Default::default()
        }
    }

    #[test]
    fn put_then_get_returns_value() {
        let mut db = Db::with_config(test_config()).unwrap();
        let _x = db.put(b"foo".to_vec(), b"bar".to_vec());
        assert_eq!(db.get(b"foo"), Some(b"bar".to_vec()));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = Db::with_config(test_config()).unwrap();
        assert_eq!(db.get(b"missing"), None);
    }

    #[test]
    fn delete_removes_key() {
        let mut db = Db::with_config(test_config()).unwrap();
        let _x = db.put(b"k".to_vec(), b"v".to_vec());
        assert!(db.delete(b"k").unwrap().is_some());
        assert_eq!(db.get(b"k"), None);
    }

    #[test]
    fn delete_missing_returns_none() {
        let mut db = Db::with_config(test_config()).unwrap();
        assert!(db.delete(b"nope").unwrap().is_none());
    }

    #[test]
    fn put_overwrites_existing_value() {
        let mut db = Db::with_config(test_config()).unwrap();
        let _x = db.put(b"k".to_vec(), b"v1".to_vec());
        let _y = db.put(b"k".to_vec(), b"v2".to_vec());
        assert_eq!(db.get(b"k"), Some(b"v2".to_vec()));
    }
}
