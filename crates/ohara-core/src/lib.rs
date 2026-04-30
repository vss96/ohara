//! Core orchestration types and traits for ohara.
//!
//! No concrete storage / embedding / git / parsing impls live here. Only
//! the contracts (`Storage`, `EmbeddingProvider`) and the orchestrators
//! (`Indexer`, `Retriever`) that depend on them.

pub mod embed;
pub mod error;
pub mod query;
pub mod storage;
pub mod types;

pub use embed::EmbeddingProvider;
pub use error::{OhraError, Result};
pub use query::*;
pub use storage::{CommitRecord, HunkHit, HunkRecord, Storage, Vector};
pub use types::*;
