//! Intermediate record types for the 5-stage indexer pipeline.
//!
//! None of these types carry behavior. They are the typed seams
//! between stages so each stage can be tested in isolation and the
//! coordinator can be generic over the concrete stage implementations.

pub mod attribute;
pub mod commit_walk;
pub mod embed;
pub mod hunk_chunk;
pub mod persist;
#[cfg(test)]
pub(crate) mod test_helpers;

pub use attribute::AttributedHunk;
pub use commit_walk::CommitWatermark;
pub use embed::EmbeddedHunk;
pub use hunk_chunk::HunkRecord;
