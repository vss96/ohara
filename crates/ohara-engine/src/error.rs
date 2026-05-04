use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("repo not indexed: {path}")]
    NoIndex { path: String },
    #[error("index needs rebuild: {reason}")]
    NeedsRebuild { reason: String },
    /// The requested IPC method is not yet implemented in this daemon version.
    /// Callers should fall back to in-process logic; this is not an internal error.
    #[error("not implemented: {method}")]
    NotImplemented { method: &'static str },
    /// Errors from ohara-core. ohara-core's own `Other` variant wraps `anyhow::Error`;
    /// this is an inherited surface and ohara-engine does not itself add `anyhow` to its
    /// dep list. Cleaning up `OhraError::Other` is tracked separately.
    #[error("ohara-core: {0}")]
    Core(#[from] ohara_core::OhraError),
    #[error("ohara-storage: {0}")]
    Storage(String),
    #[error("ohara-git: {0}")]
    Git(String),
    #[error("ohara-embed: {0}")]
    Embed(String),
    #[error("internal: {0}")]
    Internal(String),
}
