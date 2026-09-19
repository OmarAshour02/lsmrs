use crate::bloom::BloomFilter;
use crate::hash::hash64;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, RwLock};

const BLOCK_SIZE: usize = 1024 * 4;
const MAGIC: &[u8; 6] = b"LSMRS\0";
const FORMAT_VERSION: u8 = 1;
const TRAILER_SIZE: i64 = 32;

pub struct SSTableSet {
    sstables: Arc<RwLock<Arc<Vec<Arc<SSTable>>>>>,
    dir: PathBuf,
    bits_per_key: usize,
    next_seq: AtomicU64,
}
pub struct SSTable {
    path: PathBuf,
    file: File,
    first_seq: u64,
    last_seq: u64,
    sparse_index: Vec<(Vec<u8>, u64)>,
    index_start: u64,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    filter: BloomFilter,
}

enum Found {
    Value(Vec<u8>),
    Tombstone,
}

impl SSTableSet {
    pub fn open(dir: PathBuf, bits_per_key: usize) -> Result<Self, io::Error> {
        let mut spans: Vec<((u64, u64), PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("sst") => match parse_span(&path) {
                    Some(span) => spans.push((span, path)),
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} does not follow the SSTable naming scheme",
                                path.display()
                            ),
                        ));
                    }
                },
                // A `.tmp` never reached its rename, so its contents were
                // never published and nothing can reference it.
                Some("tmp") => std::fs::remove_file(&path)?,
                _ => {}
            }
        }
        spans.sort();

        let next_seq = spans
            .iter()
            .map(|((_, last), _)| last + 1)
            .max()
            .unwrap_or(0);

        // A span contained in another span belongs to a compaction whose
        // output was published before its inputs were deleted.
        let mut sstables = Vec::new();
        for (i, ((first, last), path)) in spans.iter().enumerate() {
            let superseded = spans
                .iter()
                .enumerate()
                .any(|(j, ((other_first, other_last), _))| {
                    j != i && other_first <= first && last <= other_last
                });
            if superseded {
                std::fs::remove_file(path)?;
            } else {
                sstables.push(Arc::new(SSTable::open(path.clone(), *first, *last)?));
            }
        }

        Ok(Self {
            sstables: Arc::new(RwLock::new(Arc::new(sstables))),
            dir,
            bits_per_key,
            next_seq: AtomicU64::new(next_seq),
        })
    }

    pub fn write(&self, memtable: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Result<(), io::Error> {
        if memtable.is_empty() {
            return Ok(());
        }

        let seq = self.next_seq.load(AtomicOrdering::Relaxed);
        let table = self.write_table(
            (seq, seq),
            memtable.len(),
            memtable.iter().map(|(k, v)| (k.as_slice(), v.as_deref())),
        )?;

        self.next_seq.store(seq + 1, AtomicOrdering::Relaxed);
        self.replace_tables(|tables| tables.push(Arc::new(table)));
        Ok(())
    }

    // Readers only ever clone the outer `Arc`, so the `Vec` they are holding
    // can never change underneath them. Publishing a change means building a
    // fresh `Vec` and swapping the pointer, not mutating the shared one.
    fn replace_tables<F>(&self, edit: F)
    where
        F: FnOnce(&mut Vec<Arc<SSTable>>),
    {
        let mut current = self.sstables.write().unwrap();
        let mut next = current.as_ref().clone();
        edit(&mut next);
        *current = Arc::new(next);
    }

    fn snapshot(&self) -> Arc<Vec<Arc<SSTable>>> {
        self.sstables.read().unwrap().clone()
    }

    fn write_table<'a, I>(
        &self,
        span: (u64, u64),
        num_keys: usize,
        entries: I,
    ) -> Result<SSTable, io::Error>
    where
        I: Iterator<Item = (&'a [u8], Option<&'a [u8]>)>,
    {
        let (first_seq, last_seq) = span;
        let path = self
            .dir
            .join(format!("sstable-{first_seq:06}-{last_seq:06}.sst"));
        let tmp_path = path.with_extension("sst.tmp");
        let mut writer = io::BufWriter::new(File::create(&tmp_path)?);

        let mut sparse_index: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut filter = BloomFilter::new(num_keys, self.bits_per_key);
        let mut min_key = Vec::new();
        let mut max_key = Vec::new();

        let mut current_offset: usize = 0;
        let mut block_start_offset = 0;

        for (key, value) in entries {
            if current_offset == 0 {
                min_key = key.to_vec();
            }
            max_key = key.to_vec();
            filter.insert(hash64(key));

            if block_start_offset == current_offset {
                sparse_index.push((key.to_vec(), current_offset as u64));
            }

            let (op_type, value_bytes): (u8, &[u8]) = match value {
                Some(v) => (0, v),
                None => (1, &[]),
            };

            writer.write_all(&(key.len() as u32).to_le_bytes())?;
            writer.write_all(key)?;
            writer.write_all(&(value_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(value_bytes)?;
            writer.write_all(&[op_type])?;

            current_offset += 9 + key.len() + value_bytes.len();

            if current_offset - block_start_offset >= BLOCK_SIZE {
                block_start_offset = current_offset;
            }
        }

        let index_start_offset = current_offset;

        // `current_offset` tracked the data blocks only, so each trailing
        // block's size has to be accumulated separately to locate the next.
        let mut filter_start_offset = index_start_offset;
        for (key, offset) in sparse_index.iter() {
            writer.write_all(&(key.len() as u32).to_le_bytes())?;
            writer.write_all(key)?;
            writer.write_all(&offset.to_le_bytes())?;
            filter_start_offset += 4 + key.len() + 8;
        }

        writer.write_all(&filter.num_bits().to_le_bytes())?;
        writer.write_all(&filter.num_probes().to_le_bytes())?;
        for word in filter.words() {
            writer.write_all(&word.to_le_bytes())?;
        }
        let meta_start_offset = filter_start_offset + 12 + filter.words().len() * 8;

        writer.write_all(&(min_key.len() as u32).to_le_bytes())?;
        writer.write_all(&min_key)?;
        writer.write_all(&(max_key.len() as u32).to_le_bytes())?;
        writer.write_all(&max_key)?;

        writer.write_all(&(index_start_offset as u64).to_le_bytes())?;
        writer.write_all(&(filter_start_offset as u64).to_le_bytes())?;
        writer.write_all(&(meta_start_offset as u64).to_le_bytes())?;
        writer.write_all(MAGIC)?;
        writer.write_all(&[FORMAT_VERSION, 0])?;
        writer.flush()?;
        writer.into_inner()?.sync_all()?;
        publish(&tmp_path, &path, &self.dir)?;

        Ok(SSTable {
            file: File::open(&path)?,
            path,
            first_seq,
            last_seq,
            sparse_index,
            index_start: index_start_offset as u64,
            min_key,
            max_key,
            filter,
        })
    }

    pub fn compact(&self, start: usize, count: usize) -> Result<(), io::Error> {
        assert!(count >= 2, "a compaction needs at least two tables");

        // The run is cloned out from under the lock so the merge -- by far the
        // slowest part -- runs without blocking a single reader.
        let tables = self.snapshot();
        assert!(start + count <= tables.len(), "run is out of range");
        let run = &tables[start..start + count];

        let span = (run[0].first_seq, run[count - 1].last_seq);

        // Only the oldest table in the set can drop tombstones: anywhere else
        // an older table may still hold the value the tombstone is hiding.
        let merged = merge(run, start == 0)?;

        let replacement = match merged.is_empty() {
            true => None,
            false => Some(Arc::new(self.write_table(
                span,
                merged.len(),
                merged.iter().map(|(k, v)| (k.as_slice(), v.as_deref())),
            )?)),
        };

        let mut obsolete = Vec::new();
        self.replace_tables(|tables| {
            obsolete = tables.splice(start..start + count, replacement).collect();
        });

        // Safe while readers still hold these: unlink drops the name, and the
        // inode outlives it until the last descriptor closes.
        for table in obsolete {
            std::fs::remove_file(table.path())?;
        }

        Ok(())
    }

    // Size-tiered: only merge tables of comparable size, so a small flush is
    // never rewritten into a large table. Returns the index range of the
    // oldest run long enough to be worth merging.
    pub fn pick_run(&self, min_run: usize, size_ratio: f64) -> Option<(usize, usize)> {
        let tables = self.snapshot();
        let sizes: Vec<u64> = tables
            .iter()
            .map(|t| t.file.metadata().map(|m| m.len()).unwrap_or(0))
            .collect();

        let mut start = 0;
        while start < sizes.len() {
            let mut end = start + 1;
            while end < sizes.len() {
                let smallest = sizes[start..=end].iter().copied().min().unwrap_or(0) as f64;
                let largest = sizes[start..=end].iter().copied().max().unwrap_or(0) as f64;
                if largest > smallest * size_ratio {
                    break;
                }
                end += 1;
            }
            if end - start >= min_run {
                return Some((start, end - start));
            }
            start = if end > start + 1 { end } else { start + 1 };
        }
        None
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, io::Error> {
        let hash = hash64(key);

        // Newest table wins, so search in reverse creation order and stop at
        // the first table that has an answer.
        for sstable in self.snapshot().iter().rev() {
            match sstable.get(key, hash)? {
                Some(Found::Value(value)) => return Ok(Some(value)),
                Some(Found::Tombstone) => return Ok(None),
                None => continue,
            }
        }
        Ok(None)
    }
}

impl SSTable {
    pub fn span(&self) -> (u64, u64) {
        (self.first_seq, self.last_seq)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(path: PathBuf, first_seq: u64, last_seq: u64) -> Result<Self, io::Error> {
        let mut file = File::open(&path)?;

        file.seek(SeekFrom::End(-TRAILER_SIZE))?;
        let mut trailer = [0u8; TRAILER_SIZE as usize];
        file.read_exact(&mut trailer)?;

        if &trailer[24..30] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not an lsmrs SSTable", path.display()),
            ));
        }
        if trailer[30] != FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has SSTable format version {}, expected {}",
                    path.display(),
                    trailer[30],
                    FORMAT_VERSION
                ),
            ));
        }

        let index_start = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let filter_start = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
        let meta_start = u64::from_le_bytes(trailer[16..24].try_into().unwrap());

        // Metadata block: [min_key][max_key], each length-prefixed.
        file.seek(SeekFrom::Start(meta_start))?;
        let min_key = read_prefixed(&mut file)?;
        let max_key = read_prefixed(&mut file)?;

        file.seek(SeekFrom::Start(filter_start))?;
        let mut header = [0u8; 12];
        file.read_exact(&mut header)?;
        let num_bits = u64::from_le_bytes(header[0..8].try_into().unwrap());
        let num_probes = u32::from_le_bytes(header[8..12].try_into().unwrap());

        let mut words = vec![0u64; (num_bits / 64) as usize];
        let mut word_buf = [0u8; 8];
        for word in words.iter_mut() {
            file.read_exact(&mut word_buf)?;
            *word = u64::from_le_bytes(word_buf);
        }
        let filter = BloomFilter::from_parts(words, num_bits, num_probes);

        // Sparse index block spans [index_start, filter_start).
        // Each entry is [key_len: u32][key][offset: u64].
        file.seek(SeekFrom::Start(index_start))?;
        let mut sparse_index = Vec::new();
        let mut pos = index_start;
        while pos < filter_start {
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
            filter,
            first_seq,
            last_seq,
            file,
        })
    }

    fn records(&self) -> Records<'_> {
        Records {
            table: self,
            block: 0,
            buf: Vec::new(),
            cursor: 0,
        }
    }

    fn get(&self, key: &[u8], hash: u64) -> Result<Option<Found>, io::Error> {
        if key < self.min_key.as_slice() || key > self.max_key.as_slice() {
            return Ok(None);
        }

        if !self.filter.contains(hash) {
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

        let mut block = vec![0u8; (block_end - block_start) as usize];
        self.file.read_exact_at(&mut block, block_start)?;
        let mut reader = io::Cursor::new(&block[..]);

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

struct Records<'a> {
    table: &'a SSTable,
    block: usize,
    buf: Vec<u8>,
    cursor: usize,
}

type Record = (Vec<u8>, Option<Vec<u8>>);

impl Records<'_> {
    fn next_record(&mut self) -> Result<Option<Record>, io::Error> {
        while self.cursor >= self.buf.len() {
            let Some((_, block_start)) = self.table.sparse_index.get(self.block) else {
                return Ok(None);
            };
            let block_end = match self.table.sparse_index.get(self.block + 1) {
                Some((_, next_start)) => *next_start,
                None => self.table.index_start,
            };

            self.buf = vec![0u8; (block_end - block_start) as usize];
            self.table.file.read_exact_at(&mut self.buf, *block_start)?;
            self.cursor = 0;
            self.block += 1;
        }

        let mut reader = io::Cursor::new(&self.buf[self.cursor..]);
        let key = read_prefixed(&mut reader)?;
        let value = read_prefixed(&mut reader)?;
        let mut op_type = [0u8; 1];
        reader.read_exact(&mut op_type)?;
        self.cursor += 9 + key.len() + value.len();

        match op_type[0] {
            0 => Ok(Some((key, Some(value)))),
            1 => Ok(Some((key, None))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid op_type byte in SSTable record",
            )),
        }
    }
}

impl Iterator for Records<'_> {
    type Item = Result<Record, io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

fn merge(tables: &[Arc<SSTable>], drop_tombstones: bool) -> Result<Vec<Record>, io::Error> {
    let mut iters: Vec<_> = tables.iter().map(|t| t.records().peekable()).collect();
    let mut merged: Vec<Record> = Vec::new();

    loop {
        for iter in iters.iter_mut() {
            if matches!(iter.peek(), Some(Err(_))) {
                return Err(iter.next().unwrap().unwrap_err());
            }
        }

        let mut smallest: Option<&Vec<u8>> = None;
        for iter in iters.iter_mut() {
            if let Some(Ok((key, _))) = iter.peek()
                && smallest.is_none_or(|s| key < s)
            {
                smallest = Some(key);
            }
        }
        let Some(key) = smallest.cloned() else {
            return Ok(merged);
        };

        // Tables arrive oldest first, so a later table's value overwrites an
        // earlier one and the newest write survives.
        let mut newest = None;
        for iter in iters.iter_mut() {
            if matches!(iter.peek(), Some(Ok((k, _))) if *k == key) {
                newest = Some(iter.next().unwrap()?.1);
            }
        }

        let value = newest.expect("the smallest key came from some table");
        if value.is_none() && drop_tombstones {
            continue;
        }
        merged.push((key, value));
    }
}

fn parse_span(path: &Path) -> Option<(u64, u64)> {
    let stem = path.file_stem()?.to_str()?;
    let (first, last) = stem.strip_prefix("sstable-")?.split_once('-')?;
    Some((first.parse().ok()?, last.parse().ok()?))
}

// The rename is what makes a merge visible, and it only runs once the bytes
// are durable -- so a file under its final name is always complete. The
// directory fsync is what makes the rename itself survive a crash.
fn publish(tmp: &Path, path: &Path, dir: &Path) -> Result<(), io::Error> {
    std::fs::rename(tmp, path)?;
    File::open(dir)?.sync_all()
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

    const BITS_PER_KEY: usize = 10;

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
    fn trailer_points_at_each_block() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("b", Some("2"))]))
            .unwrap();

        let bytes = std::fs::read(dir.join("sstable-000000-000000.sst")).unwrap();
        let trailer = &bytes[bytes.len() - TRAILER_SIZE as usize..];
        let index_start = u64::from_le_bytes(trailer[0..8].try_into().unwrap()) as usize;
        let filter_start = u64::from_le_bytes(trailer[8..16].try_into().unwrap()) as usize;
        let meta_start = u64::from_le_bytes(trailer[16..24].try_into().unwrap()) as usize;

        // Two records of [4][1][4][1][1] = 11 bytes each.
        assert_eq!(index_start, 22);
        // One index entry covers both records: [4][1][8].
        assert_eq!(filter_start, index_start + 13);
        assert_eq!(meta_start, filter_start + 20);
        // Metadata is min_key then max_key, each length-prefixed.
        assert_eq!(
            &bytes[meta_start..bytes.len() - TRAILER_SIZE as usize],
            b"\x01\0\0\0a\x01\0\0\0b"
        );

        assert_eq!(&trailer[24..30], MAGIC);
        assert_eq!(trailer[30], FORMAT_VERSION);
    }

    #[test]
    fn foreign_file_is_rejected() {
        let dir = temp_dir();
        let path = dir.join("sstable-000000-000000.sst");
        std::fs::write(&path, vec![0u8; 128]).unwrap();

        let err = SSTable::open(path, 0, 0)
            .err()
            .expect("open should have failed");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("not an lsmrs SSTable"));
    }

    #[test]
    fn wrong_format_version_is_rejected() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1"))])).unwrap();

        let path = dir.join("sstable-000000-000000.sst");
        let mut bytes = std::fs::read(&path).unwrap();
        let version_at = bytes.len() - 2;
        bytes[version_at] = FORMAT_VERSION + 1;
        std::fs::write(&path, bytes).unwrap();

        let err = SSTable::open(path, 0, 0)
            .err()
            .expect("open should have failed");
        assert!(err.to_string().contains("format version"));
    }

    #[test]
    fn filter_survives_reopen() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        let entries = wide_memtable(300);
        set.write(&entries).unwrap();

        let reopened = SSTable::open(dir.join("sstable-000000-000000.sst"), 0, 0).unwrap();
        assert_eq!(
            reopened.filter.num_bits(),
            set.snapshot()[0].filter.num_bits()
        );
        assert_eq!(
            reopened.filter.num_probes(),
            set.snapshot()[0].filter.num_probes()
        );

        for key in entries.keys() {
            assert!(reopened.filter.contains(hash64(key)), "filter lost a key");
        }
    }

    #[test]
    fn reads_continue_during_compaction() {
        let dir = temp_dir();
        let set = Arc::new(SSTableSet::open(dir, BITS_PER_KEY).unwrap());
        set.write(&wide_memtable(300)).unwrap();
        set.write(&memtable(&[("zzz", Some("last"))])).unwrap();

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let set = Arc::clone(&set);
                std::thread::spawn(move || {
                    for _ in 0..300 {
                        for i in (0..300).step_by(7) {
                            let key = format!("key{:05}", i).into_bytes();
                            assert_eq!(
                                set.get(&key).unwrap(),
                                Some(format!("{:0>64}", i).into_bytes()),
                                "reader lost key {i} mid-compaction"
                            );
                        }
                    }
                })
            })
            .collect();

        // Runs while all four readers are mid-scan, and unlinks both input
        // files underneath them.
        set.compact(0, 2).unwrap();

        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(set.snapshot().len(), 1);
    }

    #[test]
    fn pick_run_groups_only_similar_sizes() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();

        set.write(&wide_memtable(400)).unwrap();
        for i in 0..3 {
            set.write(&memtable(&[(&format!("k{i}"), Some("v"))]))
                .unwrap();
        }

        // The large table must not be dragged into a merge with three tiny
        // ones -- that is the write amplification size-tiering exists to avoid.
        assert_eq!(set.pick_run(3, 1.5), Some((1, 3)));
        assert_eq!(set.pick_run(4, 1.5), None);
    }

    #[test]
    fn pick_run_needs_a_full_run() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1"))])).unwrap();
        set.write(&memtable(&[("b", Some("2"))])).unwrap();

        assert_eq!(set.pick_run(4, 1.5), None);
        assert_eq!(set.pick_run(2, 1.5), Some((0, 2)));
    }

    #[test]
    fn sstable_set_can_be_shared_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SSTableSet>();
        assert_send_sync::<SSTable>();
    }

    #[test]
    fn compaction_keeps_the_newest_value() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("k", Some("old")), ("only_old", Some("x"))]))
            .unwrap();
        set.write(&memtable(&[("k", Some("new"))])).unwrap();

        set.compact(0, 2).unwrap();

        assert_eq!(set.snapshot().len(), 1);
        assert_eq!(set.snapshot()[0].span(), (0, 1));
        assert_eq!(set.get(b"k").unwrap(), Some(b"new".to_vec()));
        assert_eq!(set.get(b"only_old").unwrap(), Some(b"x".to_vec()));
        assert!(dir.join("sstable-000000-000001.sst").exists());
        assert!(!dir.join("sstable-000000-000000.sst").exists());
        assert!(!dir.join("sstable-000001-000001.sst").exists());
    }

    #[test]
    fn oldest_compaction_drops_tombstones() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("k", Some("live"))])).unwrap();
        set.write(&memtable(&[("k", None)])).unwrap();

        set.compact(0, 2).unwrap();

        // Every record was a delete of a key this merge also owned, so there
        // is nothing left to write and no output file at all.
        assert!(set.snapshot().is_empty());
        assert!(!dir.join("sstable-000000-000001.sst").exists());
        assert_eq!(set.get(b"k").unwrap(), None);
    }

    #[test]
    fn tombstone_is_carried_when_an_older_table_remains() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("k", Some("live"))])).unwrap();
        set.write(&memtable(&[("other", Some("1"))])).unwrap();
        set.write(&memtable(&[("k", None)])).unwrap();

        set.compact(1, 2).unwrap();

        // Dropping the tombstone here would expose "live" in table 0 again.
        assert_eq!(set.snapshot().len(), 2);
        assert_eq!(set.get(b"k").unwrap(), None);
        assert_eq!(set.get(b"other").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn compaction_preserves_every_key() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&wide_memtable(300)).unwrap();
        set.write(&memtable(&[("zzz", Some("last"))])).unwrap();

        set.compact(0, 2).unwrap();

        assert_eq!(set.snapshot().len(), 1);
        for i in 0..300 {
            let key = format!("key{:05}", i).into_bytes();
            assert_eq!(
                set.get(&key).unwrap(),
                Some(format!("{:0>64}", i).into_bytes()),
                "lost key {i}"
            );
        }
        assert_eq!(set.get(b"zzz").unwrap(), Some(b"last".to_vec()));
    }

    #[test]
    fn records_iterates_every_block_in_order() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&wide_memtable(300)).unwrap();

        let records: Vec<_> = set.snapshot()[0]
            .records()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(records.len(), 300);
        assert!(records.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(records[0].0, b"key00000".to_vec());
    }

    #[test]
    fn compaction_leftovers_are_deleted_on_open() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1"))])).unwrap();
        set.write(&memtable(&[("b", Some("2"))])).unwrap();

        // Stand in for a compaction that published its output and crashed
        // before deleting the two tables it consumed.
        std::fs::copy(
            dir.join("sstable-000000-000000.sst"),
            dir.join("sstable-000000-000001.sst"),
        )
        .unwrap();

        let reopened = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();

        assert_eq!(reopened.snapshot().len(), 1);
        assert_eq!(reopened.snapshot()[0].span(), (0, 1));
        assert!(!dir.join("sstable-000000-000000.sst").exists());
        assert!(!dir.join("sstable-000001-000001.sst").exists());
    }

    #[test]
    fn stray_tmp_files_are_removed_on_open() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1"))])).unwrap();

        let tmp = dir.join("sstable-000001-000001.sst.tmp");
        std::fs::write(&tmp, b"half a table").unwrap();

        let reopened = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();

        assert_eq!(reopened.snapshot().len(), 1);
        assert!(!tmp.exists());
    }

    #[test]
    fn sequence_numbers_continue_after_reopen() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1"))])).unwrap();
        set.write(&memtable(&[("b", Some("2"))])).unwrap();

        let reopened = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        assert_eq!(reopened.next_seq.load(AtomicOrdering::Relaxed), 2);

        reopened.write(&memtable(&[("c", Some("3"))])).unwrap();
        assert!(dir.join("sstable-000002-000002.sst").exists());
    }

    #[test]
    fn unrecognised_sst_name_is_rejected() {
        let dir = temp_dir();
        std::fs::write(dir.join("sstable-000000.sst"), b"old scheme").unwrap();

        let err = SSTableSet::open(dir, BITS_PER_KEY)
            .err()
            .expect("open should have failed");
        assert!(err.to_string().contains("naming scheme"));
    }

    #[test]
    fn write_then_open_roundtrips() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("b", Some("2"))]))
            .unwrap();

        let reopened = SSTable::open(dir.join("sstable-000000-000000.sst"), 0, 0).unwrap();
        let written = &set.snapshot()[0];

        assert_eq!(reopened.min_key, written.min_key);
        assert_eq!(reopened.max_key, written.max_key);
        assert_eq!(reopened.index_start, written.index_start);
        assert_eq!(reopened.sparse_index, written.sparse_index);
    }

    #[test]
    fn finds_key_that_is_not_first_in_its_block() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&memtable(&[
            ("a", Some("1")),
            ("b", Some("2")),
            ("c", Some("3")),
        ]))
        .unwrap();

        // Only "a" is indexed, so "c" is only reachable by scanning forward.
        let found = set.snapshot()[0].get(b"c", hash64(b"c")).unwrap();
        assert_eq!(value_of(found), Some(b"3".to_vec()));
    }

    #[test]
    fn missing_key_inside_range_is_not_found() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("a", Some("1")), ("c", Some("3"))]))
            .unwrap();

        // "b" sorts between the two, so the scan has to overshoot and give up
        // rather than returning "c".
        assert!(set.snapshot()[0].get(b"b", hash64(b"b")).unwrap().is_none());
        assert!(
            set.snapshot()[0]
                .get(b"zz", hash64(b"zz"))
                .unwrap()
                .is_none()
        );
        assert!(set.snapshot()[0].get(b"0", hash64(b"0")).unwrap().is_none());
    }

    #[test]
    fn spans_multiple_blocks() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        let entries = wide_memtable(300);
        set.write(&entries).unwrap();

        let sstable = &set.snapshot()[0];
        assert!(
            sstable.sparse_index.len() > 1,
            "expected several blocks, got {} index entries",
            sstable.sparse_index.len()
        );

        // Every key must be reachable, including ones in the middle and last
        // blocks where `block_end` comes from the next index entry.
        for i in 0..300 {
            let key = format!("key{:05}", i).into_bytes();
            let found = sstable.get(&key, hash64(&key)).unwrap();
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
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("k", Some("old")), ("only_old", Some("x"))]))
            .unwrap();
        set.write(&memtable(&[("k", Some("new"))])).unwrap();

        assert_eq!(set.get(b"k").unwrap(), Some(b"new".to_vec()));
        assert_eq!(set.get(b"only_old").unwrap(), Some(b"x".to_vec()));
    }

    #[test]
    fn tombstone_in_newer_table_hides_older_value() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir, BITS_PER_KEY).unwrap();
        set.write(&memtable(&[("k", Some("live"))])).unwrap();
        set.write(&memtable(&[("k", None)])).unwrap();

        assert_eq!(set.get(b"k").unwrap(), None);
    }

    #[test]
    fn empty_memtable_writes_nothing() {
        let dir = temp_dir();
        let set = SSTableSet::open(dir.clone(), BITS_PER_KEY).unwrap();
        set.write(&BTreeMap::new()).unwrap();

        assert!(set.snapshot().is_empty());
        assert!(!dir.join("sstable-000000-000000.sst").exists());
    }
}
