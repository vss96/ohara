//! ohara-engine: in-process retrieval engine shared by `ohara-cli`,
//! `ohara-mcp`, and the `ohara serve` daemon. Owns the embedder,
//! reranker, per-repo storage handles, and the LRU caches.

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod engine;
mod error;
mod handle;

pub use engine::FindPatternResult;
pub use engine::RetrievalEngine;
pub use error::EngineError;
pub use handle::RepoHandle;

pub type Result<T> = std::result::Result<T, EngineError>;
