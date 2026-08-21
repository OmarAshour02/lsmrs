use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

const BLOCK_SIZE: usize = 1024 * 4;

pub struct SSTableSet {
    sstables: Vec<SSTable>,
    dir: PathBuf,
}
pub struct SSTable {
    path: PathBuf,
    sparse_index: Vec<(Vec<u8>, u64)>,
    index_start: u64,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
}

enum Found {
    Value(Vec<u8>),
    Tombstone,
}

impl SSTableSet {
    pub fn open(dir: PathBuf) -> Result<Self, io::Error> {
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sst") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut sstables = Vec::with_capacity(paths.len());
        for path in paths {
            sstables.push(SSTable::open(path)?);
        }

        Ok(Self { sstables, dir })
    }

    pub fn write(
        &mut self,
        memtable: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<(), io::Error> {
        if memtable.is_empty() {
            return Ok(());
        }

        let mut sparse_index: Vec<(Vec<u8>, u64)> = Vec::new();

        let mut current_offset: usize = 0;
        let mut block_start_offset = 0;

        let seq = self.sstables.len();
        let name = format!("sstable-{:06}.sst", seq);
        let path = self.dir.join(name);
        let file = File::create(&path)?;
        let mut writer = io::BufWriter::new(file);

        let min_key = memtable.first_key_value().unwrap().0.clone();
        let max_key = memtable.last_key_value().unwrap().0.clone();

        for (key, value) in memtable.iter() {
            if block_start_offset == current_offset {
                sparse_index.push((key.clone(), current_offset as u64));
            }

            let (op_type, value_bytes): (u8, &[u8]) = match value {
                Some(v) => (0, v),
                None => (1, &[]),
            };

            writer.write_all(&(key.len() as u32).to_le_bytes())?; // 4 bytes
            writer.write_all(key)?; // key.len() bytes
            writer.write_all(&(value_bytes.len() as u32).to_le_bytes())?; // 4 bytes
            writer.write_all(value_bytes)?; // value.len() bytes
            writer.write_all(&[op_type])?; // 1 byte

            let record_size = 9 + key.len() + value_bytes.len();
            current_offset += record_size;

            if current_offset - block_start_offset >= BLOCK_SIZE {
                block_start_offset = current_offset;
            }
        }

        let index_start_offset = current_offset;

        // `current_offset` tracked the data blocks only, so the index block's
        // size has to be accumulated separately to locate the metadata.
        let mut meta_start_offset = index_start_offset;
        for (key, offset) in sparse_index.iter() {
            writer.write_all(&(key.len() as u32).to_le_bytes())?; // 4 bytes
            writer.write_all(key)?; // key.len() bytes
            writer.write_all(&offset.to_le_bytes())?; // 8 bytes
            meta_start_offset += 4 + key.len() + 8;
        }

        writer.write_all(&(min_key.len() as u32).to_le_bytes())?; // 4 bytes
        writer.write_all(&min_key)?; // min_key.len() bytes
        writer.write_all(&(max_key.len() as u32).to_le_bytes())?; // 4 bytes
        writer.write_all(&max_key)?; // max_key.len() bytes

        writer.write_all(&(index_start_offset as u64).to_le_bytes())?; // 8 bytes
        writer.write_all(&(meta_start_offset as u64).to_le_bytes())?; // 8 bytes
        writer.flush()?;
        writer.into_inner()?.sync_all()?;

        self.sstables.push(SSTable {
            path,
            sparse_index,
            index_start: index_start_offset as u64,
            min_key,
            max_key,
        });

        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
        // Newest table wins, so search in reverse creation order and stop at
        // the first table that has an answer.
        for sstable in self.sstables.iter().rev() {
            match sstable.get(key)? {
                Some(Found::Value(value)) => return Ok(Some(value)),
                Some(Found::Tombstone) => return Ok(None),
                None => continue,
            }
        }
        Ok(None)
    }
}

impl SSTable {
    fn open(path: PathBuf) -> Result<Self, io::Error> {
        let mut file = File::open(&path)?;

        // Trailer is the last 16 bytes: [index_start: u64][meta_start: u64].
        file.seek(SeekFrom::End(-16))?;
        let mut trailer = [0u8; 16];
        file.read_exact(&mut trailer)?;
        let index_start = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let meta_start = u64::from_le_bytes(trailer[8..16].try_into().unwrap());

        // Metadata block: [min_key][max_key], each length-prefixed.
        file.seek(SeekFrom::Start(meta_start))?;
        let min_key = read_prefixed(&mut file)?;
        let max_key = read_prefixed(&mut file)?;

        // Sparse index block spans [index_start, meta_start).
        // Each entry is [key_len: u32][key][offset: u64].
        file.seek(SeekFrom::Start(index_start))?;
        let mut sparse_index = Vec::new();
        let mut pos = index_start;
        while pos < meta_start {
            let key = read_prefixed(&mut file)?;
            let mut offset_buf = [0u8; 8];
            file.read_exact(&mut offset_buf)?;
            pos += 4 + key.len() as u64 + 8;
            sparse_index.push((key, u64::from_le_bytes(offset_buf)));
        }

        Ok(Self {
            path,
            sparse_index,
            index_start,
            min_key,
            max_key,
        })
    }

    fn get(&self, key: &[u8]) -> Result<Option<Found>, io::Error> {
        if key < self.min_key.as_slice() || key > self.max_key.as_slice() {
            return Ok(None);
        }

        // The index is sparse, so this finds the block the key would live in,
        // not the key itself.
        let candidate = self
            .sparse_index
            .partition_point(|(indexed_key, _)| indexed_key.as_slice() <= key);
        let Some(block) = candidate.checked_sub(1) else {
            return Ok(None);
        };

        let block_start = self.sparse_index[block].1;
        let block_end = match self.sparse_index.get(block + 1) {
            Some((_, next_offset)) => *next_offset,
            None => self.index_start,
        };

        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(block_start))?;
        let mut reader = io::BufReader::new(file);

        let mut pos = block_start;
        while pos < block_end {
            let record_key = read_prefixed(&mut reader)?;
            let value = read_prefixed(&mut reader)?;
            let mut op_type = [0u8; 1];
            reader.read_exact(&mut op_type)?;
            pos += (9 + record_key.len() + value.len()) as u64;

            match record_key.as_slice().cmp(key) {
                Ordering::Less => continue,
                // Records are sorted, so a larger key means ours isn't here.
                Ordering::Greater => break,
                Ordering::Equal => {
                    return match op_type[0] {
                        0 => Ok(Some(Found::Value(value))),
                        1 => Ok(Some(Found::Tombstone)),
                        _ => Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid op_type byte in SSTable record",
                        )),
                    };
                }
            }
        }

        Ok(None)
    }
}

fn read_prefixed<R: Read>(reader: &mut R) -> Result<Vec<u8>, io::Error> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lsmrs_sst_test_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn memtable(entries: &[(&str, Option<&str>)]) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        entries
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec())))
            .collect()
    }

    fn value_of(found: Option<Found>) -> Option<Vec<u8>> {
        match found {
            Some(Found::Value(v)) => Some(v),
            _ => None,
        }
    }

    // Enough padding that a few hundred records span several 4KB blocks and the
    // sparse index gets more than one entry.
    fn wide_memtable(count: usize) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        (0..count)
            .map(|i| {
                let key = format!("key{:05}", i).into_bytes();
                let value = format!("{:0>64}", i).into_bytes();
                (key, Some(value))
            })
            .collect()
    }

    #[test]
    fn trailer_points_at_index_and_metadata() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir.clone()).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("b", Some("2"))]))
            .unwrap();

        let bytes = std::fs::read(dir.join("sstable-000000.sst")).unwrap();
        let len = bytes.len();
        let index_start = u64::from_le_bytes(bytes[len - 16..len - 8].try_into().unwrap()) as usize;
        let meta_start = u64::from_le_bytes(bytes[len - 8..].try_into().unwrap()) as usize;

        // Two records of [4][1][4][1][1] = 11 bytes each.
        assert_eq!(index_start, 22);
        // One index entry covers both records: [4][1][8].
        assert_eq!(meta_start, index_start + 13);
        // Metadata is min_key then max_key, each length-prefixed.
        assert_eq!(
            &bytes[meta_start..],
            b"\x01\0\0\0a\x01\0\0\0b\x16\0\0\0\0\0\0\0\x23\0\0\0\0\0\0\0"
        );
    }

    #[test]
    fn write_then_open_roundtrips() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir.clone()).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("b", Some("2"))]))
            .unwrap();

        let reopened = SSTable::open(dir.join("sstable-000000.sst")).unwrap();
        let written = &set.sstables[0];

        assert_eq!(reopened.min_key, written.min_key);
        assert_eq!(reopened.max_key, written.max_key);
        assert_eq!(reopened.index_start, written.index_start);
        assert_eq!(reopened.sparse_index, written.sparse_index);
    }

    #[test]
    fn finds_key_that_is_not_first_in_its_block() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir).unwrap();
        set.write(&memtable(&[
            ("a", Some("1")),
            ("b", Some("2")),
            ("c", Some("3")),
        ]))
        .unwrap();

        // Only "a" is indexed, so "c" is only reachable by scanning forward.
        let found = set.sstables[0].get(b"c").unwrap();
        assert_eq!(value_of(found), Some(b"3".to_vec()));
    }

    #[test]
    fn missing_key_inside_range_is_not_found() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("c", Some("3"))]))
            .unwrap();

        // "b" sorts between the two, so the scan has to overshoot and give up
        // rather than returning "c".
        assert!(set.sstables[0].get(b"b").unwrap().is_none());
        assert!(set.sstables[0].get(b"zz").unwrap().is_none());
        assert!(set.sstables[0].get(b"0").unwrap().is_none());
    }

    #[test]
    fn spans_multiple_blocks() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir).unwrap();
        let entries = wide_memtable(300);
        set.write(&entries).unwrap();

        let sstable = &set.sstables[0];
        assert!(
            sstable.sparse_index.len() > 1,
            "expected several blocks, got {} index entries",
            sstable.sparse_index.len()
        );

        // Every key must be reachable, including ones in the middle and last
        // blocks where `block_end` comes from the next index entry.
        for i in 0..300 {
            let key = format!("key{:05}", i).into_bytes();
            let found = sstable.get(&key).unwrap();
            assert_eq!(
                value_of(found),
                Some(format!("{:0>64}", i).into_bytes()),
                "lost key {}",
                i
            );
        }
    }

    #[test]
    fn newer_table_shadows_older() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir).unwrap();
        set.write(&memtable(&[("k", Some("old")), ("only_old", Some("x"))]))
            .unwrap();
        set.write(&memtable(&[("k", Some("new"))])).unwrap();

        assert_eq!(set.get(b"k").unwrap(), Some(b"new".to_vec()));
        assert_eq!(set.get(b"only_old").unwrap(), Some(b"x".to_vec()));
    }

    #[test]
    fn tombstone_in_newer_table_hides_older_value() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir).unwrap();
        set.write(&memtable(&[("k", Some("live"))])).unwrap();
        set.write(&memtable(&[("k", None)])).unwrap();

        assert_eq!(set.get(b"k").unwrap(), None);
    }

    #[test]
    fn empty_memtable_writes_nothing() {
        let dir = temp_dir();
        let mut set = SSTableSet::open(dir.clone()).unwrap();
        set.write(&BTreeMap::new()).unwrap();

        assert!(set.sstables.is_empty());
        assert!(!dir.join("sstable-000000.sst").exists());
    }
}
