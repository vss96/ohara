//! Output type for the embed stage.

use super::attribute::AttributedHunk;

/// An `AttributedHunk` extended with its embedding vector, produced by
/// the embed stage.
#[derive(Debug, Clone)]
pub struct EmbeddedHunk {
    /// The upstream attributed hunk.
    pub attributed: AttributedHunk,
    /// Embedding vector for this hunk's effective semantic text.
    pub embedding: Vec<f32>,
}
