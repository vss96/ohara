//! Intermediate record types for the 5-stage indexer pipeline.
//!
//! None of these types carry behavior. They are the typed seams
//! between stages so each stage can be tested in isolation and the
//! coordinator can be generic over the concrete stage implementations.

pub mod commit_walk;
pub mod hunk_chunk;
pub mod attribute;
pub mod embed;
pub mod persist;

pub use commit_walk::CommitWatermark;
pub use hunk_chunk::HunkRecord;
pub use attribute::AttributedHunk;
pub use embed::EmbeddedHunk;
