pub struct Config {
    pub path: String,
    pub sync: bool,
    pub table_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: "data".to_string(),
            sync: true,
            table_size: 4096 * 1024,
        }
    }
}
