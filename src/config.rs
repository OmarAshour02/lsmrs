pub struct Config {
    pub path: String,
    pub sync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: "wal".into(),
            sync: true,
        }
    }
}
