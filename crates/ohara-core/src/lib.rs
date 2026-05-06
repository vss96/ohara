//! Core orchestration types and traits for ohara.
//!
//! No concrete storage / embedding / git / parsing impls live here. Only
//! the contracts (`Storage`, `EmbeddingProvider`) and the orchestrators
//! (`Indexer`, `Retriever`) that depend on them.

pub mod diff_text;
pub mod embed;
pub mod error;
pub mod explain;
pub mod hunk_attribution;
pub mod hunk_text;
pub mod ignore;
pub mod index_metadata;
pub mod indexer;
pub mod paths;
pub mod perf_trace;
pub mod query;
pub mod query_understanding;
pub mod retriever;
pub mod storage;
pub mod types;

pub use diff_text::{count_lines, truncate_diff, DIFF_EXCERPT_MAX_LINES};
pub use embed::{EmbedMode, EmbeddingProvider};
pub use error::{OhraError, Result};
pub use explain::{BlameRange, BlameSource, ExplainHit, ExplainMeta, ExplainQuery};
pub use ignore::{IgnoreFilter, LayeredIgnore, BUILT_IN_DEFAULTS};
pub use index_metadata::{
    compose_hint, runtime_metadata_from, CompatibilityStatus, RuntimeIndexMetadata,
    StoredIndexMetadata,
};
pub use indexer::stages;
pub use indexer::stages::{
    AttributedHunk, CommitWatermark, EmbeddedHunk, HunkRecord as StageHunkRecord,
};
pub use indexer::{
    CommitSource, Indexer, IndexerReport, NullProgress, PhaseTimings, ProgressSink, SymbolSource,
    MAX_ATTRIBUTABLE_SOURCE_BYTES,
};
pub use query::*;
pub use retriever::{RankingWeights, Retriever};
pub use storage::{CommitRecord, HunkHit, HunkId, HunkRecord, Storage, Vector};
pub use types::{
    AttributionKind, ChangeKind, CommitMeta, ContentHash, Hunk, HunkSymbol, Provenance, RepoId,
    Symbol, SymbolKind,
};
