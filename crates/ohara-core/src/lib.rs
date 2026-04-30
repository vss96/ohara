//! Core orchestration types and traits for ohara.
//!
//! No concrete storage / embedding / git / parsing impls live here. Only
//! the contracts (`Storage`, `EmbeddingProvider`) and the orchestrators
//! (`Indexer`, `Retriever`) that depend on them.

pub mod error;
pub mod types;

pub use error::{OhraError, Result};
pub use types::*;
