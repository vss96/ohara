//! `RetrievalEngine` — long-lived holder of the embedder, reranker,
//! and per-repo handles.

use crate::cache::BlameCache;
use crate::cache::EmbeddingCache;
use crate::cache::MetaCache;
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::embed::RerankProvider;
use ohara_core::explain::ExplainQuery;
use ohara_core::index_metadata::{
    CompatibilityStatus, RuntimeIndexMetadata, SCHEMA_VERSION, SEMANTIC_TEXT_VERSION,
};
use ohara_core::query::{PatternQuery, ResponseMeta};
use ohara_core::types::RepoId;
use ohara_core::EmbeddingProvider;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Result returned by [`RetrievalEngine::find_pattern`].
///
/// `Deserialize` is needed because the daemon client (Phase D) will
/// deserialize this from a socket response.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FindPatternResult {
    pub hits: Vec<ohara_core::query::PatternHit>,
    pub meta: ResponseMeta,
}

/// Result returned by [`RetrievalEngine::explain_change`].
///
/// Structural copy of the MCP `explain_change` envelope: `hits` are the
/// blame-derived commits (newest-first, `provenance = EXACT`), and `meta`
/// carries coverage / limitation diagnostics.  The JSON shape is
/// byte-identical to `(Vec<ExplainHit>, ExplainMeta)` returned by
/// `ohara_core::explain::explain_change`, so Phase G parity tests pass
/// without massaging.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ExplainResult {
    pub hits: Vec<ohara_core::explain::ExplainHit>,
    pub meta: ohara_core::explain::ExplainMeta,
}

pub struct RetrievalEngine {
    embedder: Arc<dyn EmbeddingProvider>,
    reranker: Arc<dyn RerankProvider>,
    repos: RwLock<HashMap<RepoId, Arc<RepoHandle>>>,
    embed_cache: EmbeddingCache,
    meta_cache: MetaCache,
    blame_cache: BlameCache,
    meta_hit_count: AtomicU64,
    /// Unix-second timestamp of the last dispatched request.
    /// Written by [`Self::touch`]; read by [`Self::idle_for`].
    last_request_at: AtomicU64,
}

impl RetrievalEngine {
    pub fn new(embedder: Arc<dyn EmbeddingProvider>, reranker: Arc<dyn RerankProvider>) -> Self {
        let model_id = embedder.model_id().to_string();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            embedder,
            reranker,
            repos: RwLock::new(HashMap::new()),
            embed_cache: EmbeddingCache::new(model_id, 256),
            meta_cache: MetaCache::new(Duration::from_secs(5)),
            blame_cache: BlameCache::new(64),
            meta_hit_count: AtomicU64::new(0),
            last_request_at: AtomicU64::new(now_unix),
        }
    }

    /// Record the current unix-second as the most recent request timestamp.
    ///
    /// Called by the socket server on every accepted request so the
    /// idle-timeout watchdog can compute how long the engine has been quiet.
    pub fn touch(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_request_at.store(now, Ordering::Relaxed);
    }

    /// Return the duration since the last request was dispatched.
    ///
    /// Saturates at zero if the clock appears to have gone backwards.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_request_at.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(last);
        Duration::from_secs(now.saturating_sub(last))
    }

    /// Returns the number of times `find_pattern` served `ResponseMeta`
    /// from the in-memory [`MetaCache`] (i.e. skipped storage).
    ///
    /// Exposed only under `#[cfg(test)]` so unit tests can assert caching
    /// behaviour without touching internal fields.
    #[cfg(test)]
    pub fn meta_hits(&self) -> u64 {
        self.meta_hit_count.load(Ordering::Relaxed)
    }

    /// Embed `text`, returning a cached `Arc<Vec<f32>>` on repeat calls.
    ///
    /// On a cache miss the embedder is called once and the result is stored
    /// before returning.  Subsequent calls with the same text string bypass
    /// the embedder entirely.
    pub async fn embed_query(&self, text: &str) -> crate::Result<Arc<Vec<f32>>> {
        if let Some(hit) = self.embed_cache.get(text) {
            return Ok(hit);
        }
        let mut out = self
            .embedder
            .embed_batch(&[text.to_string()])
            .await
            .map_err(EngineError::from)?;
        let v = Arc::new(
            out.pop()
                .ok_or_else(|| EngineError::Embed("embed_batch returned no vectors".into()))?,
        );
        self.embed_cache.put(text, v.clone());
        Ok(v)
    }

    pub fn embedder(&self) -> Arc<dyn EmbeddingProvider> {
        self.embedder.clone()
    }

    pub fn reranker(&self) -> Arc<dyn RerankProvider> {
        self.reranker.clone()
    }

    /// Open (or return cached) per-repo handle. Cheap on hit; on miss it
    /// canonicalises the path, derives the repo id, opens SQLite, builds the
    /// retriever, and caches the result.
    pub async fn open_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<Arc<RepoHandle>> {
        let canonical = std::fs::canonicalize(repo_path.as_ref())
            .map_err(|e| EngineError::Internal(format!("canonicalize: {e}")))?;

        let walker = ohara_git::GitWalker::open(&canonical)
            .map_err(|e| EngineError::Git(format!("open walker: {e}")))?;
        let first = walker
            .first_commit_sha()
            .map_err(|e| EngineError::Git(format!("first_commit_sha: {e}")))?;
        let repo_id = RepoId::from_parts(&first, &canonical.to_string_lossy());

        // Fast path — handle already cached.
        if let Some(existing) = self.repos.read().await.get(&repo_id).cloned() {
            return Ok(existing);
        }

        // Slow path: open storage, build retriever, build blamer.
        tracing::debug!(repo_id = ?repo_id, path = %canonical.display(), "open_repo cache miss");
        let db_path = ohara_core::paths::index_db_path(&repo_id)
            .map_err(|e| EngineError::Internal(format!("index_db_path: {e}")))?;

        let storage: Arc<dyn ohara_core::Storage> = Arc::new(
            ohara_storage::SqliteStorage::open(&db_path)
                .await
                .map_err(|e| EngineError::Storage(format!("open: {e}")))?,
        );

        let retriever = ohara_core::Retriever::new(storage.clone(), self.embedder.clone())
            .with_reranker(self.reranker.clone());

        let blamer = Arc::new(
            ohara_git::Blamer::open(&canonical)
                .map_err(|e| EngineError::Git(format!("blamer open: {e}")))?,
        );

        let handle = Arc::new(RepoHandle {
            repo_id: repo_id.clone(),
            repo_path: canonical,
            storage,
            retriever,
            blamer,
        });

        // Re-check under write lock to avoid double-open race.
        let mut w = self.repos.write().await;
        if let Some(existing) = w.get(&repo_id).cloned() {
            return Ok(existing);
        }
        w.insert(repo_id, handle.clone());
        Ok(handle)
    }

    /// Semantic search over a repo's indexed git history.
    ///
    /// Opens (or reuses) the per-repo handle, then delegates to the
    /// retriever's three-lane pipeline (vector KNN + BM25 text + BM25
    /// symbol → RRF → optional cross-encoder rerank → recency multiplier).
    ///
    /// `ResponseMeta` is served from [`MetaCache`] when a fresh entry
    /// exists (TTL = 5 s). On a miss the meta is computed via
    /// [`compose_response_meta`] and stored in the cache before returning.
    pub async fn find_pattern(
        &self,
        repo_path: impl AsRef<Path>,
        query: PatternQuery,
    ) -> crate::Result<FindPatternResult> {
        let handle = self.open_repo(repo_path).await?;
        let now_unix = chrono::Utc::now().timestamp();
        let (hits, _profile) = handle
            .retriever
            .find_pattern_with_profile(&handle.repo_id, &query, now_unix)
            .await
            .map_err(EngineError::from)?;

        let meta = match self.meta_cache.get(&handle.repo_id) {
            Some(cached) => {
                self.meta_hit_count.fetch_add(1, Ordering::Relaxed);
                cached
            }
            None => {
                let fresh = compose_response_meta(&handle).await?;
                self.meta_cache.put(handle.repo_id.clone(), fresh.clone());
                fresh
            }
        };

        Ok(FindPatternResult { hits, meta })
    }

    /// Evict the cached [`RepoHandle`] and [`MetaCache`] entry for `repo_path`.
    ///
    /// After this call, the next [`open_repo`][Self::open_repo] for the same
    /// path will re-open storage and rebuild the handle from scratch.  The
    /// BlameCache invalidation hook lives in Phase E (Task E.1).
    pub async fn invalidate_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<()> {
        let canonical = std::fs::canonicalize(repo_path.as_ref())
            .map_err(|e| EngineError::Internal(format!("canonicalize: {e}")))?;
        let walker = ohara_git::GitWalker::open(&canonical)
            .map_err(|e| EngineError::Git(format!("open walker: {e}")))?;
        let first = walker
            .first_commit_sha()
            .map_err(|e| EngineError::Git(format!("first_commit_sha: {e}")))?;
        let repo_id = RepoId::from_parts(&first, &canonical.to_string_lossy());
        self.repos.write().await.remove(&repo_id);
        self.meta_cache.invalidate(&repo_id);
        self.blame_cache.invalidate_repo(&repo_id);
        Ok(())
    }

    /// Blame-based explain for a file + line range in a repo's git history.
    ///
    /// Opens (or reuses) the per-repo handle, then delegates to the
    /// `ohara_core::explain::explain_change` orchestrator, which:
    ///   1. Calls `Blamer::blame_range` to obtain per-commit line ownership.
    ///   2. Hydrates each commit SHA from storage.
    ///   3. Sorts hits newest-first and caps to `query.k`.
    ///
    /// Returns [`ExplainResult`] whose JSON shape matches the existing MCP
    /// `explain_change` envelope, so Phase G parity tests pass unchanged.
    pub async fn explain_change(
        &self,
        repo_path: impl AsRef<Path>,
        query: ExplainQuery,
    ) -> crate::Result<ExplainResult> {
        // TODO(plan-16 E.1 follow-up): cache `Vec<BlameRange>` in `self.blame_cache`
        // keyed by `(repo_id, query.file, head_blob_oid)` so repeated calls for the
        // same file+range skip `Blamer::blame_range`. Requires either exposing the
        // HEAD blob OID from `RepoHandle` or opening a git2::Repository from
        // `handle.repo_path` to call `head_tree.get_path(&file).id()`. The hydration
        // step (storage lookups) must still run on every call since it is cheap.
        // `invalidate_repo` already calls `self.blame_cache.invalidate_repo` (E.1).
        let handle = self.open_repo(repo_path).await?;
        let (hits, meta) = ohara_core::explain::explain_change(
            &*handle.storage,
            &*handle.blamer,
            &handle.repo_id,
            &query,
        )
        .await
        .map_err(EngineError::from)?;
        Ok(ExplainResult { hits, meta })
    }
}

/// Build the [`RuntimeIndexMetadata`] expected by the current binary.
///
/// Uses the constants from `ohara-embed` (model id, dimension, reranker id)
/// and `ohara-parse` (chunker version, parser versions). Mirrored in
/// `ohara_mcp::server::current_runtime_metadata` — the duplicate is intentional
/// until Phase G.1 rewires MCP to use this engine version.
fn current_runtime_metadata() -> RuntimeIndexMetadata {
    RuntimeIndexMetadata {
        schema_version: SCHEMA_VERSION.to_string(),
        embedding_model: ohara_embed::DEFAULT_MODEL_ID.to_string(),
        embedding_dimension: ohara_embed::DEFAULT_DIM as u32,
        reranker_model: ohara_embed::DEFAULT_RERANKER_ID.to_string(),
        chunker_version: ohara_parse::CHUNKER_VERSION.to_string(),
        semantic_text_version: SEMANTIC_TEXT_VERSION.to_string(),
        parser_versions: ohara_parse::parser_versions(),
    }
}

/// Compose a hint string from freshness state and the compatibility verdict.
///
/// Mirrored from `ohara_mcp::server::compose_hint` — the duplicate is intentional
/// until Phase G.1 rewires MCP to use this engine version.
fn compose_hint(
    st: &ohara_core::query::IndexStatus,
    compatibility: &CompatibilityStatus,
) -> Option<String> {
    let freshness_hint = if st.last_indexed_commit.is_none() {
        Some("Index not built. Run `ohara index` in this repo.".to_string())
    } else if st.commits_behind_head > 50 {
        Some(format!(
            "Index is {} commits behind HEAD. Run `ohara index`.",
            st.commits_behind_head
        ))
    } else {
        None
    };
    let compat_hint = match compatibility {
        CompatibilityStatus::Compatible => None,
        CompatibilityStatus::QueryCompatibleNeedsRefresh { reason } => Some(format!(
            "Index is query-compatible but stale ({reason}). Run `ohara index --force` to refresh."
        )),
        CompatibilityStatus::NeedsRebuild { reason } => Some(format!(
            "Index needs rebuild ({reason}). Run `ohara index --rebuild` — find_pattern will refuse to run until then."
        )),
        CompatibilityStatus::Unknown { missing_components } => Some(format!(
            "Index has no recorded metadata for {}. Run `ohara index --force` to record current versions.",
            missing_components.join(", ")
        )),
    };
    match (freshness_hint, compat_hint) {
        (None, None) => None,
        (Some(f), None) => Some(f),
        (None, Some(c)) => Some(c),
        (Some(f), Some(c)) => Some(format!("{f} {c}")),
    }
}

/// Compute a fresh [`ResponseMeta`] for `handle` by querying storage for
/// index status and metadata, then assessing compatibility.
///
/// Called on a MetaCache miss inside [`RetrievalEngine::find_pattern`].
async fn compose_response_meta(handle: &RepoHandle) -> crate::Result<ResponseMeta> {
    let behind = ohara_git::GitCommitsBehind::open(&handle.repo_path)
        .map_err(|e| EngineError::Git(format!("commits_behind open: {e}")))?;
    let st =
        ohara_core::query::compute_index_status(handle.storage.as_ref(), &handle.repo_id, &behind)
            .await
            .map_err(EngineError::from)?;
    let runtime = current_runtime_metadata();
    let stored = handle
        .storage
        .get_index_metadata(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    let compatibility = CompatibilityStatus::assess(&runtime, &stored);
    let hint = compose_hint(&st, &compatibility);
    Ok(ResponseMeta {
        index_status: st,
        hint,
        compatibility: Some(compatibility),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
/// Helpers used by engine.rs tests and reachable from sibling test modules (e.g. server.rs).
pub(crate) mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Serialises tests that mutate `OHARA_HOME` (a process-global env var).
    /// Mirrors the pattern used in `ohara-core/src/paths.rs` and `ohara-cli` tests.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) struct DummyEmbedder;

    #[async_trait::async_trait]
    impl ohara_core::EmbeddingProvider for DummyEmbedder {
        fn dimension(&self) -> usize {
            384
        }

        fn model_id(&self) -> &str {
            "dummy"
        }

        async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
        }
    }

    pub(crate) struct DummyReranker;

    #[async_trait::async_trait]
    impl ohara_core::embed::RerankProvider for DummyReranker {
        async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
            Ok(vec![0.0; candidates.len()])
        }
    }

    pub(crate) fn make_test_engine() -> RetrievalEngine {
        RetrievalEngine::new(Arc::new(DummyEmbedder), Arc::new(DummyReranker))
    }

    pub(crate) fn build_test_repo(dir: &Path) {
        use git2::{Repository, Signature};
        let repo = Repository::init(dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn one() {}\n").unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.rs")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
    }

    // env_lock is held across awaits intentionally: OHARA_HOME must remain
    // stable from indexing through open_repo so both resolve the same DB path.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn explain_change_returns_one_blame_range_for_single_commit_repo() {
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", ohara_home.path());
        build_test_repo(tmp.path());

        // Index the repo so storage has the commit metadata the explain
        // orchestrator needs to hydrate blame ranges into ExplainHits.
        // Canonicalize matches the path that open_repo will derive below.
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        {
            let walker = ohara_git::GitWalker::open(&canonical).unwrap();
            let first = walker.first_commit_sha().unwrap();
            let repo_id =
                ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
            let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
            let storage: Arc<dyn ohara_core::Storage> =
                Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
            let commit_src = ohara_git::GitCommitSource::open(&canonical).unwrap();
            let symbol_src = ohara_parse::GitSymbolSource::open(&canonical).unwrap();
            let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
            indexer
                .run(&repo_id, &commit_src, &symbol_src)
                .await
                .unwrap();
        }

        let engine = make_test_engine();
        let q = ohara_core::explain::ExplainQuery {
            file: "a.rs".into(),
            line_start: 1,
            line_end: 1,
            k: 5,
            include_diff: false,
            include_related: false,
        };
        let out = engine.explain_change(&canonical, q).await.expect("explain");
        // Single-commit repo: line 1 of a.rs blames to the only commit.
        assert_eq!(out.hits.len(), 1, "expected exactly one blame hit");
    }

    #[tokio::test]
    async fn embed_query_uses_cache_on_repeat_call() {
        use std::sync::Mutex;
        struct Counting {
            calls: Mutex<usize>,
        }
        #[async_trait::async_trait]
        impl ohara_core::EmbeddingProvider for Counting {
            fn dimension(&self) -> usize {
                384
            }
            fn model_id(&self) -> &str {
                "counting"
            }
            async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
                *self.calls.lock().expect("not poisoned") += 1;
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
        }
        let counting = std::sync::Arc::new(Counting {
            calls: Mutex::new(0),
        });
        let engine = RetrievalEngine::new(counting.clone(), std::sync::Arc::new(DummyReranker));
        let _ = engine.embed_query("hello").await.expect("first");
        let _ = engine.embed_query("hello").await.expect("second");
        assert_eq!(
            *counting.calls.lock().expect("not poisoned"),
            1,
            "second call must hit cache"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn find_pattern_meta_cached_within_ttl() {
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", ohara_home.path());
        build_test_repo(tmp.path());
        let engine = make_test_engine();
        let q = ohara_core::query::PatternQuery {
            query: "hello".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let _r1 = engine
            .find_pattern(tmp.path(), q.clone())
            .await
            .expect("first call");
        let hits_before = engine.meta_hits();
        let _r2 = engine
            .find_pattern(tmp.path(), q)
            .await
            .expect("second call");
        let hits_after = engine.meta_hits();
        assert_eq!(
            hits_after - hits_before,
            1,
            "second call must hit MetaCache"
        );
    }

    #[tokio::test]
    async fn find_pattern_returns_empty_hits_on_empty_index() {
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        {
            let _g = env_lock();
            std::env::set_var("OHARA_HOME", ohara_home.path());
        }
        build_test_repo(tmp.path());
        let engine = make_test_engine();
        let q = ohara_core::query::PatternQuery {
            query: "hello".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let out = engine
            .find_pattern(tmp.path(), q)
            .await
            .expect("find_pattern");
        assert!(
            out.hits.is_empty(),
            "empty index → empty hits, got {:?}",
            out.hits
        );
    }

    #[tokio::test]
    async fn open_repo_caches_handle_by_repo_id() {
        // `env_lock` serialises tests that mutate OHARA_HOME (process-global).
        // Drop the guard before the first await to satisfy clippy::await_holding_lock.
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        {
            let _g = env_lock();
            std::env::set_var("OHARA_HOME", ohara_home.path());
        }
        build_test_repo(tmp.path());

        let engine = make_test_engine();
        let h1 = engine.open_repo(tmp.path()).await.expect("first open");
        let h2 = engine.open_repo(tmp.path()).await.expect("second open");
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "open_repo must return the cached Arc, not a fresh handle"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn invalidate_repo_drops_handle_and_meta() {
        let _g = env_lock();
        let ohara_home = tempfile::tempdir().unwrap();
        std::env::set_var("OHARA_HOME", ohara_home.path());
        let tmp = tempfile::tempdir().unwrap();
        build_test_repo(tmp.path());
        let engine = make_test_engine();
        let h1 = engine.open_repo(tmp.path()).await.expect("first open");
        engine
            .invalidate_repo(tmp.path())
            .await
            .expect("invalidate");
        let h2 = engine.open_repo(tmp.path()).await.expect("second open");
        assert!(
            !Arc::ptr_eq(&h1, &h2),
            "invalidated handle must be re-opened, got the same Arc"
        );
    }
}
