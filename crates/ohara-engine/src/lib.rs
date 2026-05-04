//! ohara-engine: in-process retrieval engine shared by `ohara-cli`,
//! `ohara-mcp`, and the `ohara serve` daemon. Owns the embedder,
//! reranker, per-repo storage handles, and the LRU caches.

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cache;
mod engine;
mod error;
mod handle;
pub mod ipc;
pub mod registry;
pub mod server;

pub use cache::EmbeddingCache;
pub use cache::MetaCache;
pub use engine::ExplainResult;
pub use engine::FindPatternResult;
pub use engine::RetrievalEngine;
pub use error::EngineError;
pub use handle::RepoHandle;
pub use server::serve_unix;

pub type Result<T> = std::result::Result<T, EngineError>;
