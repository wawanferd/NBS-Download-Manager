use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server responded with HTTP {0}")]
    Status(u16),
    #[error("connection closed before the segment was complete")]
    Incomplete,
    #[error("{0}")]
    Protocol(String),
    #[error("download was cancelled")]
    Cancelled,
    #[error("download not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

impl Error {
    /// Whether retrying the same request later has a reasonable chance of succeeding.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(e) => !e.is_builder() && !e.is_redirect(),
            Error::Status(code) => *code >= 500 || *code == 408 || *code == 429,
            Error::Incomplete => true,
            _ => false,
        }
    }
}
