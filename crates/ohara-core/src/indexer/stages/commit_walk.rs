//! Output type for the commit-walk stage.

use crate::types::CommitMeta;

#[cfg(test)]
mod walk_tests {
    use super::*;
    use crate::indexer::CommitSource;
    use crate::{OhraError, Result};
    use async_trait::async_trait;
    use crate::types::{Hunk, CommitMeta};

    struct VecSource(Vec<CommitMeta>);

    #[async_trait]
    impl CommitSource for VecSource {
        async fn list_commits(
            &self,
            since: Option<&str>,
        ) -> Result<Vec<CommitMeta>> {
            match since {
                None => Ok(self.0.clone()),
                Some(sha) => {
                    let pos = self.0.iter().position(|m| m.commit_sha == sha);
                    match pos {
                        None => Ok(self.0.clone()),
                        Some(i) => Ok(self.0[..i].to_vec()),
                    }
                }
            }
        }

        async fn hunks_for_commit(
            &self,
            _sha: &str,
        ) -> Result<Vec<Hunk>> {
            Ok(vec![])
        }
    }

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 0,
            message: "m".into(),
        }
    }

    #[tokio::test]
    async fn empty_source_yields_empty_output() {
        let src = VecSource(vec![]);
        let out = CommitWalkStage::run(&src, None).await.unwrap();
        assert!(out.is_empty(), "empty source must yield empty commit list");
    }

    #[tokio::test]
    async fn returns_all_commits_when_no_watermark() {
        let src = VecSource(vec![meta("aaa"), meta("bbb")]);
        let out = CommitWalkStage::run(&src, None).await.unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn resumes_after_watermark() {
        // Commits are stored newest-first (aaa > bbb > ccc).
        // Watermark at "bbb" means "bbb and ccc are indexed; return
        // only aaa".
        let src = VecSource(vec![meta("aaa"), meta("bbb"), meta("ccc")]);
        let wm = CommitWatermark::new("bbb");
        let out = CommitWalkStage::run(&src, Some(&wm)).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].commit_sha, "aaa");
    }
}

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
