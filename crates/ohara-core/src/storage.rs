use crate::query::IndexStatus;
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::Result;
use async_trait::async_trait;

/// Vector with the same dimension as `EmbeddingProvider::dimension()`.
pub type Vector = Vec<f32>;

/// Hunk primary-key id used as the join key across the three retrieval lanes
/// (vector KNN, FTS5 hunk text, FTS5 symbol name) before Reciprocal Rank
/// Fusion. Matches the `hunk.id` SQLite rowid type.
pub type HunkId = i64;

#[derive(Debug, Clone)]
pub struct HunkRecord {
    pub hunk: Hunk,
    pub diff_emb: Vector,
}

#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub meta: CommitMeta,
    pub message_emb: Vector,
}

#[derive(Debug, Clone)]
pub struct HunkHit {
    /// Storage-side primary key of the hunk row. Used by the retrieval
    /// pipeline to dedup across lanes via Reciprocal Rank Fusion.
    pub hunk_id: HunkId,
    pub hunk: Hunk,
    pub commit: CommitMeta,
    pub similarity: f32,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn open_repo(&self, repo_id: &RepoId, path: &str, first_commit_sha: &str) -> Result<()>;

    async fn get_index_status(&self, repo_id: &RepoId) -> Result<IndexStatus>;

    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> Result<()>;

    async fn put_commit(&self, repo_id: &RepoId, record: &CommitRecord) -> Result<()>;

    async fn put_hunks(&self, repo_id: &RepoId, records: &[HunkRecord]) -> Result<()>;

    async fn put_head_symbols(&self, repo_id: &RepoId, symbols: &[Symbol]) -> Result<()>;

    async fn knn_hunks(
        &self,
        repo_id: &RepoId,
        query_emb: &[f32],
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;

    /// BM25-ranked hunks whose `diff_text` matches `query` via FTS5.
    /// Ordered best-first; `similarity` is a positive normalized score
    /// (`1.0 / (1.0 + (-bm25_raw))`) so callers can keep the
    /// "higher is better" convention.
    async fn bm25_hunks_by_text(
        &self,
        repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;

    /// BM25-ranked hunks whose touched files contain a symbol whose name
    /// (or a sibling-merged name from the AST chunker) matches `query`.
    /// Ordered best-first.
    async fn bm25_hunks_by_symbol_name(
        &self,
        repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;

    async fn blob_was_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<bool>;

    async fn record_blob_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<()>;
}
