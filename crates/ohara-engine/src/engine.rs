//! `RetrievalEngine` — long-lived holder of the embedder, reranker,
//! and per-repo handles.

use crate::cache::BlameCache;
use crate::cache::EmbeddingCache;
use crate::cache::MetaCache;
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::embed::RerankProvider;
use ohara_core::explain::ExplainQuery;
use ohara_core::index_metadata::CompatibilityStatus;
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
    /// Plan 21 E.1: counts BlameCache hits in `explain_change`.
    blame_cache_hit_count: AtomicU64,
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
            blame_cache_hit_count: AtomicU64::new(0),
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

    /// Returns the number of times `explain_change` served blame ranges
    /// from the in-memory [`BlameCache`] (i.e., skipped `Blamer::blame_range`).
    ///
    /// Exposed only under `#[cfg(test)]` so unit tests can assert caching
    /// behaviour without touching internal fields.
    #[cfg(test)]
    pub fn blame_cache_hits(&self) -> u64 {
        self.blame_cache_hit_count.load(Ordering::Relaxed)
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
    /// `compose_response_meta` and stored in the cache before returning.
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
    /// Opens (or reuses) the per-repo handle. On a BlameCache hit (same
    /// HEAD blob OID for the file), skips `Blamer::blame_range` and feeds
    /// the cached `Vec<BlameRange>` directly to the hydrator. On a miss,
    /// calls the blamer, caches the result, then hydrates.
    ///
    /// Returns [`ExplainResult`] whose JSON shape matches the existing MCP
    /// `explain_change` envelope, so parity tests pass unchanged.
    pub async fn explain_change(
        &self,
        repo_path: impl AsRef<Path>,
        query: ExplainQuery,
    ) -> crate::Result<ExplainResult> {
        let handle = self.open_repo(repo_path).await?;
        let content_hash_opt = compute_head_content_hash(&handle.repo_path, &query.file);

        // Cache hit path — skip Blamer entirely.
        if let Some(ref hash) = content_hash_opt {
            if let Some(cached) = self
                .blame_cache
                .get(&handle.repo_id, &query.file, hash.as_str())
            {
                self.blame_cache_hit_count.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(file = %query.file, "explain_change: BlameCache hit");
                let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
                    ohara_core::explain::hydrator::HydrateInputs {
                        storage: &*handle.storage,
                        blame_ranges: (*cached).clone(),
                        query: &query,
                        repo_id: &handle.repo_id,
                    },
                )
                .await
                .map_err(EngineError::from)?;
                return Ok(assemble_explain_result(hydrated, &query));
            }
        }

        // Cache miss: blame → cache → hydrate.
        use ohara_core::explain::BlameSource;
        let raw_ranges = handle
            .blamer
            .blame_range(&query.file, query.line_start, query.line_end)
            .await
            .map_err(EngineError::from)?;

        if let Some(hash) = content_hash_opt {
            self.blame_cache.put(
                handle.repo_id.clone(),
                query.file.clone(),
                hash.as_str().to_string(),
                Arc::new(raw_ranges.clone()),
            );
        }

        let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
            ohara_core::explain::hydrator::HydrateInputs {
                storage: &*handle.storage,
                blame_ranges: raw_ranges,
                query: &query,
                repo_id: &handle.repo_id,
            },
        )
        .await
        .map_err(EngineError::from)?;
        Ok(assemble_explain_result(hydrated, &query))
    }
}

/// Compute a fresh [`ResponseMeta`] for `handle` by querying storage for
/// index status and metadata, then assessing compatibility.
///
/// Called on a MetaCache miss inside [`RetrievalEngine::find_pattern`].
///
/// Issue #40: query-time callers don't know the user's `--embed-cache`
/// intent (no flag at query time), so we adopt the stored
/// `embed_input_mode` for the runtime expectation when present. An
/// internally-consistent `--embed-cache=diff` index then assesses as
/// `Compatible`, not `NeedsRebuild`. Mirrors the same override in
/// `ohara-cli`'s `status` command.
async fn compose_response_meta(handle: &RepoHandle) -> crate::Result<ResponseMeta> {
    let behind = ohara_git::GitCommitsBehind::open(&handle.repo_path)
        .map_err(|e| EngineError::Git(format!("commits_behind open: {e}")))?;
    let st =
        ohara_core::query::compute_index_status(handle.storage.as_ref(), &handle.repo_id, &behind)
            .await
            .map_err(EngineError::from)?;
    let stored = handle
        .storage
        .get_index_metadata(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    let mut runtime = crate::current_runtime_metadata(ohara_core::EmbedMode::default());
    if let Some(stored_mode) = stored.components.get("embed_input_mode") {
        runtime.embed_input_mode = stored_mode.clone();
    }
    let compatibility = CompatibilityStatus::assess(&runtime, &stored);
    let hint = ohara_core::index_metadata::compose_hint(&st, &compatibility);
    Ok(ResponseMeta {
        index_status: st,
        hint,
        compatibility: Some(compatibility),
    })
}

/// Compute the git blob OID for `file` at HEAD in `repo_path`.
///
/// Returns `None` when the file is absent from HEAD (deleted file,
/// wrong path) or when the git2 repository can't be opened. Callers
/// treat `None` as "skip the cache, let the Blamer decide".
fn compute_head_content_hash(
    repo_path: &std::path::Path,
    file: &str,
) -> Option<ohara_core::types::ContentHash> {
    let repo = git2::Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(file)).ok()?;
    Some(ohara_core::types::ContentHash::from_blob_oid(entry.id()))
}

/// Assemble `ExplainResult` from a `HydratedBlame` + the original query.
///
/// Applies sort-newest-first + truncate-to-k. Used by both the cache-hit
/// and cache-miss paths so the logic lives in one place.
/// K_MAX mirrors `ohara_core::explain::K_MAX` (20); duplicated here as a
/// private constant rather than re-exporting to keep the dependency surface
/// narrow (plan-21 risks: "K_MAX constant duplication").
fn assemble_explain_result(
    mut hydrated: ohara_core::explain::hydrator::HydratedBlame,
    query: &ohara_core::explain::ExplainQuery,
) -> ExplainResult {
    const K_MAX: u8 = 20;
    hydrated
        .hits
        .sort_by(|a, b| match b.commit_date.cmp(&a.commit_date) {
            std::cmp::Ordering::Equal => a.commit_sha.cmp(&b.commit_sha),
            other => other,
        });
    let k = query.k.clamp(1, K_MAX) as usize;
    hydrated.hits.truncate(k);
    let commits_unique = hydrated.hits.len();
    ExplainResult {
        hits: hydrated.hits,
        meta: ohara_core::explain::ExplainMeta {
            lines_queried: hydrated.clamped_range,
            commits_unique,
            blame_coverage: hydrated.coverage,
            limitation: hydrated.limitation,
            related_commits: hydrated.related_commits,
            enrichment_limitation: hydrated.enrichment_limitation,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
/// Helpers used by engine.rs tests and reachable from sibling test modules (e.g. server.rs).
#[path = "engine_tests.rs"]
pub(crate) mod tests;
