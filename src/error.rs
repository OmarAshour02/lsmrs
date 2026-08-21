use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("key not found")]
    KeyNotFound,

    #[error("invalid command: {0}")]
    InvalidCommand(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DbError>;
