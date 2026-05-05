//! Unit tests for the coordinator pipeline.

use super::*;
use crate::index_metadata::StoredIndexMetadata;
use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkHit, HunkId, HunkRecord as StorageHunkRecord};
use crate::types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

// --- Minimal fakes reused across coordinator tests ---

struct SingleCommitSource {
    sha: String,
    hunks: Vec<Hunk>,
}

#[async_trait]
impl crate::indexer::CommitSource for SingleCommitSource {
    async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
        Ok(vec![CommitMeta {
            commit_sha: self.sha.clone(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_000_000,
            message: "add feature".into(),
        }])
    }
    async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
        Ok(self.hunks.clone())
    }
}

struct NoopSymbolSource;
#[async_trait]
impl crate::indexer::SymbolSource for NoopSymbolSource {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
        Ok(vec![])
    }
}

struct ZeroEmbedder {
    dim: usize,
}
#[async_trait]
impl EmbeddingProvider for ZeroEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        "zero"
    }
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }
}

#[derive(Default)]
struct SpyStorage {
    put_commit_calls: Mutex<Vec<String>>,
    put_hunk_totals: Mutex<Vec<usize>>,
    watermark: Mutex<Option<String>>,
    seen_commits: Mutex<Vec<String>>,
}

#[async_trait]
impl Storage for SpyStorage {
    async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
        Ok(IndexStatus {
            last_indexed_commit: self.watermark.lock().unwrap().clone(),
            commits_behind_head: 0,
            indexed_at: None,
        })
    }
    async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> Result<()> {
        Ok(())
    }
    async fn put_commit(&self, _: &RepoId, meta: &CommitRecord) -> Result<()> {
        self.put_commit_calls
            .lock()
            .unwrap()
            .push(meta.meta.commit_sha.clone());
        self.seen_commits
            .lock()
            .unwrap()
            .push(meta.meta.commit_sha.clone());
        Ok(())
    }
    async fn commit_exists(&self, sha: &str) -> Result<bool> {
        // If we have a watermark matching sha, consider it already indexed.
        let wm = self.watermark.lock().unwrap().clone();
        Ok(wm.as_deref() == Some(sha)
            || self.seen_commits.lock().unwrap().contains(&sha.to_string()))
    }
    async fn put_hunks(&self, _: &RepoId, rows: &[StorageHunkRecord]) -> Result<()> {
        self.put_hunk_totals.lock().unwrap().push(rows.len());
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
    async fn get_hunk_symbols_batch(
        &self,
        _: &RepoId,
        _: &[HunkId],
    ) -> Result<std::collections::HashMap<HunkId, Vec<HunkSymbol>>> {
        Ok(std::collections::HashMap::new())
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

fn hunk(sha: &str) -> Hunk {
    use crate::types::ChangeKind;
    Hunk {
        commit_sha: sha.into(),
        file_path: "src/lib.rs".into(),
        language: None,
        change_kind: ChangeKind::Added,
        diff_text: "+fn x() {}\n".into(),
    }
}

#[tokio::test]
async fn coordinator_indexes_single_commit_end_to_end() {
    let storage = Arc::new(SpyStorage::default());
    let coordinator = Coordinator::new(storage.clone(), Arc::new(ZeroEmbedder { dim: 4 }));
    let repo = RepoId::from_parts("sha", "/repo");
    let source = SingleCommitSource {
        sha: "abc".into(),
        hunks: vec![hunk("abc")],
    };
    coordinator
        .run(&repo, &source, &NoopSymbolSource)
        .await
        .unwrap();

    assert_eq!(
        *storage.put_commit_calls.lock().unwrap(),
        vec!["abc"],
        "coordinator must persist exactly one commit"
    );
    assert_eq!(
        *storage.put_hunk_totals.lock().unwrap(),
        vec![1],
        "coordinator must persist one hunk"
    );
}

#[tokio::test]
async fn coordinator_resumes_skipping_already_indexed_commit() {
    // Watermark is already at "abc" — the commit source still
    // returns "abc" but the coordinator must skip it.
    let storage = Arc::new(SpyStorage {
        watermark: Mutex::new(Some("abc".into())),
        ..Default::default()
    });
    let coordinator = Coordinator::new(storage.clone(), Arc::new(ZeroEmbedder { dim: 4 }));
    let repo = RepoId::from_parts("sha", "/repo");
    let source = SingleCommitSource {
        sha: "abc".into(),
        hunks: vec![hunk("abc")],
    };
    coordinator
        .run(&repo, &source, &NoopSymbolSource)
        .await
        .unwrap();

    assert!(
        storage.put_commit_calls.lock().unwrap().is_empty(),
        "coordinator must not re-index an already-indexed commit"
    );
}

#[tokio::test]
async fn coordinator_resume_from_attributed_hunks_directly() {
    use crate::indexer::stages::attribute::AttributedHunk;
    use crate::indexer::stages::hunk_chunk::HunkRecord;

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });
    let coordinator = Coordinator::new(storage.clone(), embedder);

    let repo = RepoId::from_parts("sha", "/repo");
    let commit = CommitMeta {
        commit_sha: "abc".into(),
        parent_sha: None,
        is_merge: false,
        author: None,
        ts: 1_000_000,
        message: "add feature".into(),
    };
    let attributed = vec![AttributedHunk {
        record: HunkRecord {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn x() {}\n".into(),
            semantic_text: "fn x() {}".into(),
            source_hunk: Hunk::default(),
        },
        symbols: None,
        attributed_semantic_text: None,
    }];

    coordinator
        .run_from_attributed(&repo, &commit, attributed)
        .await
        .unwrap();

    assert_eq!(
        *storage.put_commit_calls.lock().unwrap(),
        vec!["abc"],
        "partial-pipeline run must still persist the commit"
    );
    assert_eq!(
        *storage.put_hunk_totals.lock().unwrap(),
        vec![1],
        "partial-pipeline run must persist the hunk"
    );
}
