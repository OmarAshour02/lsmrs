use std::path::PathBuf;
pub struct Config {
    pub path: PathBuf,
    pub sync: bool,
    pub table_size: usize,
    pub bits_per_key: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: PathBuf::from("data"),
            sync: true,
            table_size: 4096 * 1024,
            bits_per_key: 10,
        }
    }
}
