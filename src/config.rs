use std::path::PathBuf;
pub struct Config {
    pub path: PathBuf,
    pub sync: bool,
    pub table_size: usize,
    pub bits_per_key: usize,
    pub compaction_threshold: usize,
    pub compaction_size_ratio: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: PathBuf::from("data"),
            sync: true,
            table_size: 4096 * 1024,
            bits_per_key: 10,
            compaction_threshold: 4,
            compaction_size_ratio: 1.5,
        }
    }
}
