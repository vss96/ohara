//! Shared test fakes: FakeStorage, FakeEmbedder, ScriptedReranker, fake_hit.

use crate::embed::RerankProvider;
use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkHit, HunkId, HunkRecord};
use crate::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

pub fn fake_hit(id: HunkId, sha: &str, ts: i64, sim: f32, diff: &str) -> HunkHit {
    HunkHit {
        hunk_id: id,
        hunk: Hunk {
            commit_sha: sha.into(),
            file_path: format!("src/{sha}.rs"),
            language: Some("rust".into()),
            change_kind: ChangeKind::Added,
            diff_text: diff.into(),
        },
        commit: CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: Some("a".into()),
            ts,
            message: format!("msg-{sha}"),
        },
        similarity: sim,
    }
}

/// Records which lanes were called and returns hard-coded `HunkHit`s
/// per method.
pub struct FakeStorage {
    pub knn: Vec<HunkHit>,
    pub fts_text: Vec<HunkHit>,
    pub fts_sym: Vec<HunkHit>,
    pub calls: Mutex<Vec<&'static str>>,
}

impl FakeStorage {
    pub fn new(knn: Vec<HunkHit>, fts_text: Vec<HunkHit>, fts_sym: Vec<HunkHit>) -> Self {
        Self {
            knn,
            fts_text,
            fts_sym,
            calls: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl crate::Storage for FakeStorage {
    async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> {
        Ok(())
    }
    async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> {
        Ok(IndexStatus {
            last_indexed_commit: None,
            commits_behind_head: 0,
            indexed_at: None,
        })
    }
    async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> {
        Ok(())
    }
    async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> {
        Ok(())
    }
    async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
        unreachable!("retriever tests should not exercise commit_exists")
    }
    async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> {
        Ok(())
    }
    async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> {
        Ok(())
    }
    async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> {
        unreachable!("retriever tests should not exercise clear_head_symbols")
    }
    async fn knn_hunks(
        &self,
        _: &RepoId,
        _: &[f32],
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> crate::Result<Vec<HunkHit>> {
        self.calls.lock().unwrap().push("knn");
        Ok(self.knn.clone())
    }
    async fn bm25_hunks_by_text(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> crate::Result<Vec<HunkHit>> {
        self.calls.lock().unwrap().push("fts_text");
        Ok(self.fts_text.clone())
    }
    async fn bm25_hunks_by_semantic_text(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> crate::Result<Vec<HunkHit>> {
        // Plan 11: keep retriever tests focused on the existing
        // three lanes until Task 4.1 wires the semantic lane in.
        self.calls.lock().unwrap().push("fts_semantic");
        Ok(Vec::new())
    }
    async fn bm25_hunks_by_symbol_name(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> crate::Result<Vec<HunkHit>> {
        self.calls.lock().unwrap().push("fts_sym");
        Ok(self.fts_sym.clone())
    }
    async fn bm25_hunks_by_historical_symbol(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> crate::Result<Vec<HunkHit>> {
        // Plan 11 Task 4.1 will exercise this lane in retriever
        // tests; default empty for now so the existing fixture
        // doesn't change behavior.
        self.calls.lock().unwrap().push("fts_hist_sym");
        Ok(Vec::new())
    }
    async fn get_hunk_symbols(
        &self,
        _: &RepoId,
        _: crate::storage::HunkId,
    ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
        Ok(Vec::new())
    }
    async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
        Ok(false)
    }
    async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
        Ok(())
    }
    async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<CommitMeta>> {
        unreachable!("retriever tests should not exercise get_commit")
    }
    async fn get_hunks_for_file_in_commit(
        &self,
        _: &RepoId,
        _: &str,
        _: &str,
    ) -> crate::Result<Vec<crate::types::Hunk>> {
        unreachable!("retriever tests should not exercise get_hunks_for_file_in_commit")
    }
    async fn get_neighboring_file_commits(
        &self,
        _: &RepoId,
        _: &str,
        _: &str,
        _: u8,
        _: u8,
    ) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> {
        Ok(Vec::new())
    }
    async fn get_index_metadata(
        &self,
        _: &RepoId,
    ) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
        Ok(crate::index_metadata::StoredIndexMetadata::default())
    }
    async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> {
        Ok(())
    }
}

pub struct FakeEmbedder;

#[async_trait]
impl crate::EmbeddingProvider for FakeEmbedder {
    fn dimension(&self) -> usize {
        4
    }
    fn model_id(&self) -> &str {
        "fake"
    }
    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
    }
}

/// Reranker that maps a fixed `diff_text -> score` table. Returns 0.0
/// for any unknown candidate so the pipeline still produces output.
pub struct ScriptedReranker {
    pub scores: HashMap<String, f32>,
}

#[async_trait]
impl RerankProvider for ScriptedReranker {
    async fn rerank(&self, _query: &str, candidates: &[&str]) -> crate::Result<Vec<f32>> {
        Ok(candidates
            .iter()
            .map(|c| *self.scores.get(*c).unwrap_or(&0.0))
            .collect())
    }
}
