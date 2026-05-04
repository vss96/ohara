//! Output type for the commit-walk stage.

use crate::types::CommitMeta;

/// The watermark a coordinator stores after successfully processing a
/// commit. Used to filter subsequent `commit_walk` output so the
/// coordinator can resume from where it left off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitWatermark {
    /// The SHA-1 hex string of the last successfully persisted commit.
    pub commit_sha: String,
}

impl CommitWatermark {
    pub fn new(commit_sha: impl Into<String>) -> Self {
        Self {
            commit_sha: commit_sha.into(),
        }
    }

    /// Returns `true` if this watermark is older than `meta`, meaning
    /// `meta` has not yet been indexed.
    pub fn is_before(&self, meta: &CommitMeta) -> bool {
        self.commit_sha != meta.commit_sha
    }
}
