use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::PathBuf;
// 4mb
const BLOCK_SIZE: usize = 1024 * 4;

pub struct SSTableSet {
    sstables: Vec<SSTable>,
    dir: PathBuf,
}
pub struct SSTable {
    path: PathBuf,
    sparse_index: Vec<(Vec<u8>, u64)>,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
}

impl SSTableSet {
    pub fn open(dir: PathBuf) -> Result<Self, io::Error> {
        Ok(Self {
            sstables: vec![],
            dir,
        })
    }

    pub fn write(
        &self,
        memtable: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<SSTable, io::Error> {
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

        for (key, offset) in sparse_index.iter() {
            writer.write_all(&(key.len() as u32).to_le_bytes())?; // 4 bytes
            writer.write_all(key)?; // key.len() bytes
            writer.write_all(&offset.to_le_bytes())?; // 8 bytes
        }

        writer.write_all(&(index_start_offset as u64).to_le_bytes())?;
        writer.flush()?;
        writer.into_inner()?.sync_all()?;

        Ok(SSTable {
            path,
            sparse_index,
            min_key,
            max_key,
        })
    }
}
