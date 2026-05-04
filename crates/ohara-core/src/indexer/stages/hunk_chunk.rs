//! Output type for the hunk-chunk stage.

use crate::types::Hunk;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::CommitSource;
    use crate::types::{CommitMeta, ChangeKind};
    use crate::Result;
    use async_trait::async_trait;

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_000_000,
            message: "add foo".into(),
        }
    }

    fn hunk(sha: &str, path: &str, diff: &str) -> Hunk {
        Hunk {
            commit_sha: sha.into(),
            file_path: path.into(),
            language: None,
            change_kind: ChangeKind::Added,
            diff_text: diff.into(),
        }
    }

    struct TwoMethodSource;

    #[async_trait]
    impl CommitSource for TwoMethodSource {
        async fn list_commits(
            &self,
            _since: Option<&str>,
        ) -> Result<Vec<CommitMeta>> {
            Ok(vec![meta("abc")])
        }

        async fn hunks_for_commit(
            &self,
            _sha: &str,
        ) -> Result<Vec<Hunk>> {
            Ok(vec![
                hunk("abc", "src/foo.rs", "+fn alpha() {}\n"),
                hunk("abc", "src/foo.rs", "+fn beta() {}\n"),
            ])
        }
    }

    #[tokio::test]
    async fn two_method_file_yields_two_hunk_records() {
        let cm = meta("abc");
        let records = HunkChunkStage::run(&TwoMethodSource, &cm).await.unwrap();
        assert_eq!(
            records.len(),
            2,
            "expected 2 HunkRecords for a 2-method synthetic commit, got {}",
            records.len()
        );
        assert_eq!(records[0].file_path, "src/foo.rs");
        assert_eq!(records[1].file_path, "src/foo.rs");
        assert!(
            records[0].diff_text.contains("alpha"),
            "first record must contain alpha hunk"
        );
        assert!(
            records[1].diff_text.contains("beta"),
            "second record must contain beta hunk"
        );
    }

    #[tokio::test]
    async fn empty_commit_yields_empty_records() {
        struct EmptySource;
        #[async_trait]
        impl CommitSource for EmptySource {
            async fn list_commits(
                &self,
                _: Option<&str>,
            ) -> Result<Vec<CommitMeta>> {
                Ok(vec![])
            }
            async fn hunks_for_commit(
                &self,
                _: &str,
            ) -> Result<Vec<Hunk>> {
                Ok(vec![])
            }
        }
        let cm = meta("abc");
        let records = HunkChunkStage::run(&EmptySource, &cm).await.unwrap();
        assert!(records.is_empty());
    }
}

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
