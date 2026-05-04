//! Output type for the commit-walk stage.

use crate::types::CommitMeta;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CommitMeta;

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 0,
            message: "msg".into(),
        }
    }

    #[test]
    fn watermark_new_round_trips_sha() {
        let w = CommitWatermark::new("cafebabe");
        assert_eq!(w.commit_sha, "cafebabe");
    }

    #[test]
    fn is_before_returns_true_for_different_sha() {
        let w = CommitWatermark::new("aaa");
        let m = meta("bbb");
        assert!(w.is_before(&m), "watermark on 'aaa' must report 'bbb' as unindexed");
    }

    #[test]
    fn is_before_returns_false_for_same_sha() {
        let w = CommitWatermark::new("aaa");
        let m = meta("aaa");
        assert!(
            !w.is_before(&m),
            "watermark matching sha must not report the commit as unindexed"
        );
    }
}

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
