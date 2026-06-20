use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("key not found")]
    KeyNotFound,

    #[error("invalid command: {0}")]
    InvalidCommand(String),
}

pub type Result<T> = std::result::Result<T, DbError>;
