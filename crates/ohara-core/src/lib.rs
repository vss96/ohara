//! Core orchestration types and traits for ohara.
//!
//! No concrete storage / embedding / git / parsing impls live here. Only
//! the contracts (`Storage`, `EmbeddingProvider`) and the orchestrators
//! (`Indexer`, `Retriever`) that depend on them.

pub mod diff_text;
pub mod embed;
pub mod error;
pub mod explain;
pub mod index_metadata;
pub mod indexer;
pub mod paths;
pub mod query;
pub mod retriever;
pub mod storage;
pub mod types;

pub use diff_text::{count_lines, truncate_diff, DIFF_EXCERPT_MAX_LINES};
pub use embed::EmbeddingProvider;
pub use error::{OhraError, Result};
pub use explain::{BlameRange, BlameSource, ExplainHit, ExplainMeta, ExplainQuery};
pub use index_metadata::{CompatibilityStatus, RuntimeIndexMetadata, StoredIndexMetadata};
pub use indexer::{
    CommitSource, Indexer, IndexerReport, NullProgress, PhaseTimings, ProgressSink, SymbolSource,
};
pub use query::*;
pub use retriever::{RankingWeights, Retriever};
pub use storage::{CommitRecord, HunkHit, HunkId, HunkRecord, Storage, Vector};
pub use types::*;
