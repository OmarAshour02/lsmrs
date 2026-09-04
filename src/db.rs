use crate::config::Config;
use crate::sstable::SSTableSet;
use crate::wal::{Operation, Wal};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io;

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
        std::fs::create_dir_all(&config.path)?;

        let mut wal = Wal::open(&config.path.join("wal.log"), config.sync)?;
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

        let sstable_set = SSTableSet::open(config.path.clone(), config.bits_per_key)?;

        Ok(Self {
            map,
            wal,
            size: 0,
            config,
            sstable_set,
        })
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.sstable_set.write(&self.map)?;
        self.map.clear();
        self.wal.truncate()?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
        match self.map.get(key) {
            Some(entry) => Ok(entry.clone()),
            None => self.sstable_set.get(key),
        }
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), io::Error> {
        self.wal.insert(&key, &value)?;
        self.size += key.len() + value.len();
        self.map.insert(key, Some(value));
        if self.size > self.config.table_size {
            self.flush()?;
            self.size = 0;
        }

        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<(), io::Error> {
        self.wal.delete(key)?;
        self.size += key.len();
        self.map.insert(key.to_vec(), None);
        if self.size > self.config.table_size {
            self.flush()?;
            self.size = 0;
        }
        Ok(())
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("lsmrs_db_test_{}_{}", std::process::id(), n))
    }

    fn config_at(path: PathBuf, table_size: usize) -> Config {
        Config {
            path,
            sync: false,
            table_size,
            bits_per_key: Config::default().bits_per_key,
        }
    }

    fn test_config() -> Config {
        config_at(temp_path(), Config::default().table_size)
    }

    // A threshold of 0 makes every put after the first trip the flush check, so
    // the SSTable paths are exercised without writing megabytes.
    const FLUSH_EVERY_PUT: usize = 0;

    #[test]
    fn put_then_get_returns_value() {
        let mut db = Db::with_config(test_config()).unwrap();
        db.put(b"foo".to_vec(), b"bar".to_vec()).unwrap();
        assert_eq!(db.get(b"foo").unwrap(), Some(b"bar".to_vec()));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = Db::with_config(test_config()).unwrap();
        assert_eq!(db.get(b"missing").unwrap(), None);
    }

    #[test]
    fn delete_removes_key() {
        let mut db = Db::with_config(test_config()).unwrap();
        db.put(b"k".to_vec(), b"v".to_vec()).unwrap();
        db.delete(b"k").unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn delete_missing_is_a_noop() {
        let mut db = Db::with_config(test_config()).unwrap();
        db.delete(b"nope").unwrap();
        assert_eq!(db.get(b"nope").unwrap(), None);
    }

    #[test]
    fn put_overwrites_existing_value() {
        let mut db = Db::with_config(test_config()).unwrap();
        db.put(b"k".to_vec(), b"v1".to_vec()).unwrap();
        db.put(b"k".to_vec(), b"v2".to_vec()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn flushed_value_is_readable() {
        let mut db = Db::with_config(config_at(temp_path(), FLUSH_EVERY_PUT)).unwrap();
        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"b".to_vec(), b"2".to_vec()).unwrap();

        // Both keys left the memtable in the flush; the read path has to find
        // them in the SSTable that was just created.
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn tombstone_hides_flushed_value() {
        let mut db = Db::with_config(config_at(temp_path(), FLUSH_EVERY_PUT)).unwrap();
        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"b".to_vec(), b"2".to_vec()).unwrap();

        // `a` now lives on disk. The tombstone is memtable-only, so the read
        // path must stop at it instead of falling through to the SSTable.
        db.delete(b"a").unwrap();
        assert_eq!(db.get(b"a").unwrap(), None);
    }

    #[test]
    fn second_flush_does_not_clobber_first() {
        let mut db = Db::with_config(config_at(temp_path(), FLUSH_EVERY_PUT)).unwrap();
        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
        db.put(b"c".to_vec(), b"3".to_vec()).unwrap();
        db.put(b"d".to_vec(), b"4".to_vec()).unwrap();

        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"c").unwrap(), Some(b"3".to_vec()));
        assert_eq!(db.get(b"d").unwrap(), Some(b"4".to_vec()));
    }

    #[test]
    fn reopen_replays_wal() {
        let path = temp_path();
        {
            let mut db = Db::with_config(config_at(path.clone(), usize::MAX)).unwrap();
            db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
            db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
            db.delete(b"a").unwrap();
        }

        let db = Db::with_config(config_at(path, usize::MAX)).unwrap();
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"a").unwrap(), None);
    }

    #[test]
    fn reopen_after_flush_reads_sstable() {
        let path = temp_path();
        {
            let mut db = Db::with_config(config_at(path.clone(), FLUSH_EVERY_PUT)).unwrap();
            db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
            db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
        }

        // The flush truncated the WAL, so this can only come from the SSTable.
        let db = Db::with_config(config_at(path, FLUSH_EVERY_PUT)).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn wal_is_empty_after_flush() {
        let path = temp_path();
        let mut db = Db::with_config(config_at(path.clone(), FLUSH_EVERY_PUT)).unwrap();
        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"b".to_vec(), b"2".to_vec()).unwrap();

        let wal_len = std::fs::metadata(path.join("wal.log")).unwrap().len();
        assert_eq!(wal_len, 0);
    }
}
