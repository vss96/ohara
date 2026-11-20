//! Error type for the harness crate.

use ohara_core::OhraError;

/// Harness-level result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("runtime: {0}")]
    Runtime(String),
    #[error("contract: {0}")]
    Contract(String),
    #[error("guard: {0}")]
    Guard(String),
    #[error("git: {0}")]
    Git(String),
    #[error("store: {0}")]
    Store(#[from] OhraError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("deadline: {0}")]
    Deadline(String),
}
