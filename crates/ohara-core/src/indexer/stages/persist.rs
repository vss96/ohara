//! Persist stage: writes one commit + its embedded hunks to storage
//! in a single logical operation.

use crate::indexer::stages::embed::EmbedOutput;
use crate::storage::{CommitRecord, HunkRecord as StorageHunkRecord};
use crate::types::{CommitMeta, RepoId};
use crate::{Result, Storage};

/// The persist stage: writes commit + embedded hunks to storage in a
/// single logical operation. The storage layer's DELETE-then-INSERT
/// contract (`commit::put`) guarantees idempotency — re-running on the
/// same SHA replays cleanly.
///
/// This stage carries no state. The coordinator calls it once per
/// successfully embedded commit.
pub struct PersistStage;

impl PersistStage {
    /// Write `commit` and all hunks in `embed_output` to `storage`.
    ///
    /// On success, the commit's watermark is ready to be advanced.
    /// On error, the storage write is incomplete — the coordinator
    /// should not advance the watermark and should propagate the error.
    pub async fn run(
        repo: &RepoId,
        commit: &CommitMeta,
        embed_output: EmbedOutput,
        storage: &dyn Storage,
    ) -> Result<()> {
        let commit_record = CommitRecord {
            meta: commit.clone(),
            message_emb: embed_output.commit_embedding,
        };
        storage.put_commit(repo, &commit_record).await?;

        let hunk_rows: Vec<StorageHunkRecord> = embed_output
            .hunks
            .into_iter()
            .map(|eh| {
                let semantic_text = eh.attributed.effective_semantic_text().to_owned();
                let symbols = eh
                    .attributed
                    .symbols
                    .map(|syms| {
                        syms.into_iter()
                            .map(|s| crate::types::HunkSymbol {
                                kind: s.kind,
                                name: s.name.clone(),
                                qualified_name: s.qualified_name.clone(),
                                attribution: crate::types::AttributionKind::ExactSpan,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                StorageHunkRecord {
                    hunk: eh.attributed.record.source_hunk,
                    diff_emb: eh.embedding,
                    semantic_text,
                    symbols,
                }
            })
            .collect();

        storage.put_hunks(repo, &hunk_rows).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_metadata::StoredIndexMetadata;
    use crate::indexer::stages::attribute::AttributedHunk;
    use crate::indexer::stages::embed::{EmbedOutput, EmbeddedHunk};
    use crate::indexer::stages::hunk_chunk::HunkRecord;
    use crate::query::IndexStatus;
    use crate::storage::{CommitRecord, HunkHit, HunkId, HunkRecord as StorageHunkRecord};
    use crate::types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol};
    use crate::{Result, Storage};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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

    fn embedded(sha: &str) -> EmbeddedHunk {
        EmbeddedHunk {
            attributed: AttributedHunk {
                record: HunkRecord {
                    commit_sha: sha.into(),
                    file_path: "f.rs".into(),
                    diff_text: "+x\n".into(),
                    semantic_text: "x".into(),
                    source_hunk: Hunk::default(),
                },
                symbols: None,
                attributed_semantic_text: None,
            },
            embedding: vec![0.1, 0.2, 0.3, 0.4],
        }
    }

    fn embed_output(sha: &str, n_hunks: usize) -> EmbedOutput {
        EmbedOutput {
            commit_embedding: vec![0.5; 4],
            hunks: (0..n_hunks).map(|_| embedded(sha)).collect(),
        }
    }

    #[derive(Default)]
    struct RecordingStorage {
        commits: Mutex<Vec<String>>,
        hunk_counts: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl Storage for RecordingStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> Result<()> {
            Ok(())
        }
        async fn put_commit(&self, _: &RepoId, record: &CommitRecord) -> Result<()> {
            self.commits
                .lock()
                .unwrap()
                .push(record.meta.commit_sha.clone());
            Ok(())
        }
        async fn commit_exists(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn put_hunks(&self, _: &RepoId, rows: &[StorageHunkRecord]) -> Result<()> {
            self.hunk_counts.lock().unwrap().push(rows.len());
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> Result<()> {
            Ok(())
        }
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> Result<Vec<HunkSymbol>> {
            Ok(vec![])
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_commit(&self, _: &RepoId, _: &str) -> Result<Option<CommitMeta>> {
            Ok(None)
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
        ) -> Result<Vec<Hunk>> {
            Ok(vec![])
        }
        async fn get_neighboring_file_commits(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
            _: u8,
            _: u8,
        ) -> Result<Vec<(u32, CommitMeta)>> {
            Ok(vec![])
        }
        async fn get_index_metadata(&self, _: &RepoId) -> Result<StoredIndexMetadata> {
            Ok(StoredIndexMetadata::default())
        }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn persist_writes_one_commit_and_correct_hunk_count() {
        let storage = Arc::new(RecordingStorage::default());
        let repo = RepoId::from_parts("sha", "/repo");
        let cm = meta("abc");
        let output = embed_output("abc", 3);
        PersistStage::run(&repo, &cm, output, storage.as_ref())
            .await
            .unwrap();
        assert_eq!(
            *storage.commits.lock().unwrap(),
            vec!["abc"],
            "must call put_commit exactly once"
        );
        assert_eq!(
            *storage.hunk_counts.lock().unwrap(),
            vec![3],
            "must call put_hunks once with all 3 hunks"
        );
    }

    #[tokio::test]
    async fn persist_is_idempotent_on_same_sha() {
        // Running persist twice for the same commit SHA must not error.
        let storage = Arc::new(RecordingStorage::default());
        let repo = RepoId::from_parts("sha", "/repo");
        let cm = meta("abc");
        PersistStage::run(&repo, &cm, embed_output("abc", 1), storage.as_ref())
            .await
            .unwrap();
        PersistStage::run(&repo, &cm, embed_output("abc", 1), storage.as_ref())
            .await
            .unwrap();
        let commits = storage.commits.lock().unwrap().clone();
        assert_eq!(
            commits,
            vec!["abc", "abc"],
            "both runs must delegate to storage"
        );
    }
}
