use crate::index_metadata::StoredIndexMetadata;
use crate::query::IndexStatus;
use crate::types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol};
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
    /// Plan 11: normalized text fed to the embedder + the
    /// `fts_hunk_semantic` BM25 lane in place of the raw diff. Falls
    /// back to `hunk.diff_text` when the semantic-text builder yields
    /// an empty body. Stored alongside the raw diff so display /
    /// provenance still shows what the user expects.
    pub semantic_text: String,
    /// Plan 11: per-hunk symbol attribution (which symbols in the file
    /// did this hunk actually touch). Empty when the parser couldn't
    /// resolve the file or when no `ExactSpan` / `HunkHeader` evidence
    /// applied; the v0.7 indexer never writes `FileFallback`-confidence
    /// rows.
    pub symbols: Vec<HunkSymbol>,
}

impl HunkRecord {
    /// Construct a v0.6-compatible record (semantic_text falls back to
    /// the raw diff text; no symbol attribution). Used by callers that
    /// haven't been ported to plan 11's richer construction yet —
    /// keeps test fakes and back-fill paths small.
    pub fn legacy(hunk: Hunk, diff_emb: Vector) -> Self {
        let semantic_text = hunk.diff_text.clone();
        Self {
            hunk,
            diff_emb,
            semantic_text,
            symbols: Vec::new(),
        }
    }
}

#[cfg(test)]
mod hunk_record_tests {
    use super::*;
    use crate::types::ChangeKind;

    #[test]
    fn legacy_constructor_seeds_semantic_text_from_diff_text_and_no_symbols() {
        // Plan 11 Task 1.2: HunkRecord::legacy is the v0.6-compat
        // shim — semantic_text mirrors diff_text byte-for-byte and
        // symbols stays empty so test mocks compile and pre-attribution
        // index passes still produce a populated semantic-text FTS row.
        let hunk = Hunk {
            commit_sha: "abc".into(),
            file_path: "src/x.rs".into(),
            language: Some("rust".into()),
            change_kind: ChangeKind::Added,
            diff_text: "+fn foo() {}\n".into(),
        };
        let rec = HunkRecord::legacy(hunk.clone(), vec![0.0_f32; 4]);
        assert_eq!(rec.semantic_text, hunk.diff_text);
        assert!(rec.symbols.is_empty());
    }
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
    // --- Repo lifecycle ---

    /// Register a repository in the index. Idempotent on `repo_id`:
    /// re-calling with the same id updates `path` / `first_commit_sha`
    /// without dropping previously indexed rows.
    async fn open_repo(&self, repo_id: &RepoId, path: &str, first_commit_sha: &str) -> Result<()>;

    /// Return the current index watermark for `repo_id`. A fresh repo
    /// returns `IndexStatus { last_indexed_commit: None, .. }`.
    async fn get_index_status(&self, repo_id: &RepoId) -> Result<IndexStatus>;

    /// Advance the watermark to `sha`. The indexer calls this after
    /// every commit it successfully indexes; it must be monotonic in
    /// caller-visible terms (callers always pass the newest commit
    /// they've persisted).
    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> Result<()>;

    // --- Write path ---

    /// Persist a commit's metadata + message embedding. Idempotent on
    /// `record.meta.commit_sha` (INSERT OR REPLACE).
    async fn put_commit(&self, repo_id: &RepoId, record: &CommitRecord) -> Result<()>;

    /// Cheap "is this commit already indexed?" check used by the indexer's
    /// per-commit short-circuit on resume.
    ///
    /// The watermark only excludes a commit and its strict ancestor chain
    /// (`git2::Revwalk::hide`). Commits reachable via a non-watermark-ancestor
    /// path — merge from a feature branch, octopus merge, history rewrite —
    /// would otherwise be re-walked and re-embedded even though their
    /// `commit_record` row already exists. Implementations should answer
    /// from the primary-key index on `commit_record.sha` so this stays
    /// sub-millisecond per commit. See plan-9 / RFC v0.6.3.
    async fn commit_exists(&self, sha: &str) -> Result<bool>;

    /// Persist a batch of hunks with their diff embeddings. Idempotent
    /// at the (commit_sha, file_path) grain — re-calling with the same
    /// hunk replaces the row rather than appending a duplicate.
    async fn put_hunks(&self, repo_id: &RepoId, records: &[HunkRecord]) -> Result<()>;

    /// Persist HEAD-snapshot symbols extracted by the AST chunker.
    /// Caller is responsible for deciding whether to clear the previous
    /// snapshot first (see `clear_head_symbols`).
    async fn put_head_symbols(&self, repo_id: &RepoId, symbols: &[Symbol]) -> Result<()>;

    /// Drop all HEAD symbol rows for `repo_id` (and the matching
    /// `vec_symbol` / `fts_symbol_name` rows). Used by `ohara index
    /// --force` before re-extracting symbols so the AST sibling-merge
    /// chunker can repopulate without duplicates.
    async fn clear_head_symbols(&self, repo_id: &RepoId) -> Result<()>;

    // --- Read lanes (retrieval) ---

    /// Vector KNN over hunk diff embeddings. Ordered best-first.
    /// `similarity = 1.0 / (1.0 + distance)` where `distance` is the
    /// L2 distance reported by sqlite-vec, giving callers a "higher
    /// is better" score in `(0, 1]`. Optional `language` and
    /// `since_unix` filters narrow the candidate set before ranking.
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

    // --- Blob cache ---

    /// Has a blob with this `(blob_sha, embedding_model)` been embedded
    /// before? Used to skip re-embedding identical content across
    /// commits.
    async fn blob_was_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<bool>;

    /// Record that `(blob_sha, embedding_model)` has been embedded.
    /// Idempotent on the pair.
    async fn record_blob_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<()>;

    // --- Explain support ---

    /// Fetch a single commit's metadata. Returns `Ok(None)` if the SHA
    /// isn't indexed (e.g., commit is older than the watermark). Used by
    /// the `explain_change` orchestrator to enrich blame results with
    /// commit message + author + date for display.
    async fn get_commit(&self, repo_id: &RepoId, sha: &str) -> Result<Option<CommitMeta>>;

    /// Fetch the hunks of a commit that touch a specific file path. Used
    /// by `explain_change` to attach a diff excerpt per blame hit.
    /// JOINs `hunk` against `file_path` filtered by sha + path.
    async fn get_hunks_for_file_in_commit(
        &self,
        repo_id: &RepoId,
        sha: &str,
        file_path: &str,
    ) -> Result<Vec<Hunk>>;

    // --- Index metadata (plan 13) ---

    /// Read every `index_metadata` row for `repo_id` as a typed
    /// `StoredIndexMetadata`. Components absent from the table are
    /// absent from the returned map (callers diagnose them as
    /// `Unknown`, not as a stored mismatch).
    async fn get_index_metadata(&self, repo_id: &RepoId) -> Result<StoredIndexMetadata>;

    /// Replace the `version` row for each `(component, version)` pair
    /// passed in, scoped to `repo_id`. Components not in `components`
    /// are left untouched — callers MUST NOT use this method to clear
    /// stale rows; they must pass the new values for everything they
    /// want to update. `recorded_at` is set to the current unix time.
    async fn put_index_metadata(
        &self,
        repo_id: &RepoId,
        components: &[(String, String)],
    ) -> Result<()>;
}
