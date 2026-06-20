use std::io::{Read, Seek, Write};
const CRC_SIZE: u32 = 4;

#[derive(Debug)]
pub enum Operation {
    Insert,
    Delete,
}

impl From<Operation> for u8 {
    fn from(operation: Operation) -> Self {
        match operation {
            Operation::Insert => 0,
            Operation::Delete => 1,
        }
    }
}

impl TryFrom<u8> for Operation {
    type Error = std::io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Operation::Insert),
            1 => Ok(Operation::Delete),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid operation byte",
            )),
        }
    }
}

#[derive(Debug)]
pub struct Record {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub operation: Operation,
}

pub struct Wal {
    file: std::fs::File,
    sync: bool,
}

impl Wal {
    pub fn open(path: &str, sync: bool) -> Result<Self, std::io::Error> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;

        Ok(Self { file, sync })
    }

    fn create_body(key: &[u8], value: &[u8], operation: Operation) -> Vec<u8> {
        let operation_byte: u8 = operation.into();

        let mut payload = Vec::new();
        payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
        payload.extend_from_slice(key);
        payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
        payload.extend_from_slice(value);
        payload.push(operation_byte);

        let checksum = crc32fast::hash(&payload);
        let length: u32 = CRC_SIZE + payload.len() as u32;

        let mut record = Vec::new();
        record.extend_from_slice(&length.to_le_bytes());
        record.extend_from_slice(&checksum.to_le_bytes());
        record.extend_from_slice(&payload);

        record
    }

    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), std::io::Error> {
        let body = Self::create_body(key, value, Operation::Insert);
        self.file.write_all(&body)?;
        if self.sync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<(), std::io::Error> {
        let body = Self::create_body(key, &[], Operation::Delete);
        self.file.write_all(&body)?;
        if self.sync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    fn parse_record(payload: &[u8]) -> Result<Record, std::io::Error> {
        let mut cursor = 0;

        let key_len = u32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;

        let key = payload[cursor..cursor + key_len].to_vec();
        cursor += key_len;

        let value_len =
            u32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;

        let value = payload[cursor..cursor + value_len].to_vec();
        cursor += value_len;

        let operation = Operation::try_from(payload[cursor])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        Ok(Record {
            key,
            value,
            operation,
        })
    }

    pub fn read(&mut self) -> Result<Vec<Record>, std::io::Error> {
        self.file.seek(std::io::SeekFrom::Start(0))?;
        let mut records = Vec::new();
        let mut reader = std::io::BufReader::new(&self.file);
        loop {
            let mut length_bytes = [0u8; 4];
            match reader.read_exact(&mut length_bytes) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let length = u32::from_le_bytes(length_bytes);
            let mut checksum_bytes = [0u8; 4];
            match reader.read_exact(&mut checksum_bytes) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let checksum = u32::from_le_bytes(checksum_bytes);
            let mut payload = vec![0u8; length as usize - CRC_SIZE as usize];
            match reader.read_exact(&mut payload) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }

            let crc = crc32fast::hash(&payload);
            if crc != checksum {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Checksum mismatch",
                ));
            }

            records.push(Self::parse_record(&payload)?);
        }

        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // Each test gets its own WAL file so they don't clobber each other or the
    // real `wal` in the project root.
    fn temp_path() -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("lsmrs_wal_test_{}_{}", std::process::id(), n))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn write_read_roundtrip() {
        let path = temp_path();
        {
            let mut wal = Wal::open(&path, false).unwrap();
            wal.insert(b"foo", b"bar").unwrap();
            wal.insert(b"omar", b"ashour").unwrap();
            wal.delete(b"foo").unwrap();
        }

        let mut wal = Wal::open(&path, false).unwrap();
        let records = wal.read().unwrap();

        assert_eq!(records.len(), 3);

        assert_eq!(records[0].key, b"foo");
        assert_eq!(records[0].value, b"bar");
        assert!(matches!(records[0].operation, Operation::Insert));

        assert_eq!(records[1].key, b"omar");
        assert_eq!(records[1].value, b"ashour");
        assert!(matches!(records[1].operation, Operation::Insert));

        assert_eq!(records[2].key, b"foo");
        assert!(records[2].value.is_empty());
        assert!(matches!(records[2].operation, Operation::Delete));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupted_record_fails_crc() {
        let path = temp_path();
        {
            let mut wal = Wal::open(&path, false).unwrap();
            wal.insert(b"foo", b"bar").unwrap();
        }

        // Flip a byte inside the value, leaving the length header intact. The
        // record is still fully present, so this is corruption, not truncation.
        let mut bytes = std::fs::read(&path).unwrap();
        let value_byte = bytes.len() - 2;
        bytes[value_byte] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let mut wal = Wal::open(&path, false).unwrap();
        let err = wal.read().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn torn_write_is_recovered() {
        let path = temp_path();
        let first_len;
        {
            let mut wal = Wal::open(&path, false).unwrap();
            wal.insert(b"foo", b"bar").unwrap();
            first_len = std::fs::metadata(&path).unwrap().len();
            wal.insert(b"omar", b"ashour").unwrap();
        }

        // Simulate a crash mid-append: truncate partway into the second record.
        let full_len = std::fs::metadata(&path).unwrap().len();
        let torn_len = first_len + (full_len - first_len) / 2;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(torn_len).unwrap();

        let mut wal = Wal::open(&path, false).unwrap();
        let records = wal.read().unwrap();

        // The complete first record survives; the torn tail is discarded.
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, b"foo");
        assert_eq!(records[0].value, b"bar");

        std::fs::remove_file(&path).ok();
    }
}
