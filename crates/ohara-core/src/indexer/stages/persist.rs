//! Persist stage: writes one commit + its embedded hunks to storage
//! as one atomic unit (`StorageWrite::put_commit_with_hunks`).

use crate::indexer::stages::embed::EmbedOutput;
use crate::storage::{CommitRecord, HunkRecord as StorageHunkRecord};
use crate::types::{CommitMeta, RepoId};
use crate::{Result, Storage};

/// Derive a ULID string for a commit. Falls back to an all-zero random
/// segment when `sha` is shorter than 40 chars or contains non-hex
/// characters (only reachable with synthetic test SHAs; production SHAs
/// are always full 40-hex).
fn commit_ulid(ts: i64, sha: &str) -> String {
    let valid_hex = sha.len() >= 40 && sha[..40].bytes().all(|b| b.is_ascii_hexdigit());
    let effective: std::borrow::Cow<str> = if valid_hex {
        std::borrow::Cow::Borrowed(sha)
    } else {
        std::borrow::Cow::Owned("0".repeat(40))
    };
    crate::ulid_for_commit(ts, &effective).to_string()
}

/// The persist stage: writes commit + embedded hunks to storage in one
/// `put_commit_with_hunks` call, which durable backends run as a single
/// transaction. That matters for resume: the walker skips any commit
/// that already has a commit row, so the row must never land without
/// its hunks. The DELETE-then-INSERT inside the write keeps a re-run
/// on the same SHA idempotent.
///
/// This stage carries no state. The coordinator calls it once per
/// successfully embedded commit.
pub struct PersistStage;

impl PersistStage {
    /// Write `commit` and all hunks in `embed_output` to `storage`.
    ///
    /// On success, the commit is fully stored. On error nothing of it is
    /// stored (with an atomic backend), so the commit counts as failed and
    /// the next pass redoes it.
    pub async fn run(
        repo: &RepoId,
        commit: &CommitMeta,
        embed_output: EmbedOutput,
        storage: &dyn Storage,
    ) -> Result<()> {
        let commit_record = CommitRecord {
            ulid: commit_ulid(commit.ts, &commit.commit_sha),
            meta: commit.clone(),
            message_emb: embed_output.commit_embedding,
        };
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

        storage
            .put_commit_with_hunks(repo, &commit_record, &hunk_rows)
            .await
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
    use crate::Result;
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
        /// `(sha, hunk count)` per `put_commit_with_hunks` call. Recorded
        /// instead of delegating, so a split write shows up as a miss.
        atomic_writes: Mutex<Vec<(String, usize)>>,
    }

    #[async_trait]
    impl crate::storage::StorageStatus for RecordingStorage {
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
        }

        async fn get_index_metadata(&self, _: &RepoId) -> Result<StoredIndexMetadata> {
            Ok(StoredIndexMetadata::default())
        }

        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl crate::storage::StorageWrite for RecordingStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
            Ok(())
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

        async fn put_commit_with_hunks(
            &self,
            _: &RepoId,
            commit: &CommitRecord,
            hunks: &[StorageHunkRecord],
        ) -> Result<()> {
            self.atomic_writes
                .lock()
                .unwrap()
                .push((commit.meta.commit_sha.clone(), hunks.len()));
            Ok(())
        }

        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl crate::storage::StorageRead for RecordingStorage {
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

        async fn get_hunk_symbols_batch(
            &self,
            _: &RepoId,
            _: &[HunkId],
        ) -> Result<std::collections::HashMap<HunkId, Vec<HunkSymbol>>> {
            Ok(std::collections::HashMap::new())
        }
    }

    #[async_trait]
    impl crate::storage::StorageExplain for RecordingStorage {
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
    }

    impl crate::storage::StorageCache for RecordingStorage {}

    #[tokio::test]
    async fn persist_writes_the_commit_and_all_its_hunks_in_one_storage_call() {
        let storage = Arc::new(RecordingStorage::default());
        let repo = RepoId::from_parts("sha", "/repo");
        let cm = meta("abc");
        let output = embed_output("abc", 3);
        PersistStage::run(&repo, &cm, output, storage.as_ref())
            .await
            .unwrap();
        assert_eq!(
            *storage.atomic_writes.lock().unwrap(),
            vec![("abc".to_string(), 3)],
            "commit and its 3 hunks must go through one put_commit_with_hunks call"
        );
        assert!(
            storage.commits.lock().unwrap().is_empty()
                && storage.hunk_counts.lock().unwrap().is_empty(),
            "split put_commit / put_hunks writes are not atomic"
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
        let writes = storage.atomic_writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            vec![("abc".to_string(), 1), ("abc".to_string(), 1)],
            "both runs must delegate to storage"
        );
    }
}
