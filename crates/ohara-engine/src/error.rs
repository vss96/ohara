use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("repo not indexed: {path}")]
    NoIndex { path: String },
    #[error("index needs rebuild: {reason}")]
    NeedsRebuild { reason: String },
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
