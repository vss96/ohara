//! Error type for the harness crate.

use ohara_core::OhraError;

/// Harness-level result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised by the harness crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid or unresolvable configuration.
    #[error("config: {0}")]
    Config(String),
    /// An agent runtime failed to launch or run.
    #[error("runtime: {0}")]
    Runtime(String),
    /// A task contract was missing or invalid.
    #[error("contract: {0}")]
    Contract(String),
    /// A guard check rejected the work.
    #[error("guard: {0}")]
    Guard(String),
    /// A git operation failed.
    #[error("git: {0}")]
    Git(String),
    /// The task store returned an error.
    #[error("store: {0}")]
    Store(#[from] OhraError),
    /// An I/O operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A deadline was exceeded or invalid.
    #[error("deadline: {0}")]
    Deadline(String),
}
