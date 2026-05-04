//! Output type for the hunk-chunk stage.

use crate::types::Hunk;

/// A single diff hunk produced by the hunk-chunk stage.
///
/// This is structurally similar to `ohara_core::Hunk` today. Keeping
/// it as a distinct type makes the stage boundary explicit and allows
/// the hunk-chunk stage to carry additional fields (e.g. parse errors)
/// without polluting the upstream `Hunk` type.
#[derive(Debug, Clone)]
pub struct HunkRecord {
    /// Commit SHA this hunk belongs to.
    pub commit_sha: String,
    /// Repo-relative path of the changed file.
    pub file_path: String,
    /// Raw unified-diff text for this hunk.
    pub diff_text: String,
    /// Pre-computed semantic text (commit message prefix + hunk body)
    /// ready for the embedding stage.
    pub semantic_text: String,
    /// Source `Hunk` retained for attribution-stage inputs.
    pub source_hunk: Hunk,
}
