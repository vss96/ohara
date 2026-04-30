use thiserror::Error;

#[derive(Debug, Error)]
pub enum OhraError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("repo not indexed: {0}")]
    RepoNotIndexed(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, OhraError>;
