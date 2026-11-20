//! `RetrievalEngine` — long-lived holder of the embedder, reranker,
//! and per-repo handles.

use crate::cache::AncestryCache;
use crate::cache::EmbeddingCache;
use crate::cache::RepoCaches;
use crate::error::EngineError;
use crate::handle::{RepoHandle, RepoStore};
use crate::index_service::{EnsureIndexedOutcome, IndexService, PruneOutcome};
use ohara_core::embed::RerankProvider;
use ohara_core::explain::ExplainQuery;
use ohara_core::index_metadata::CompatibilityStatus;
use ohara_core::query::{PatternQuery, ResponseMeta};
use ohara_core::types::RepoId;
use ohara_core::CommitSha;
use ohara_core::EmbeddingProvider;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

mod open;
pub(crate) mod visibility;

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
    /// One store per repository, shared by all of its worktrees.
    stores: RwLock<HashMap<RepoId, Arc<RepoStore>>>,
    /// One handle per worktree, keyed by canonical worktree path.
    repos: RwLock<HashMap<PathBuf, Arc<RepoHandle>>>,
    /// Repo-independent: keyed by `(model_id, query text)`.
    embed_cache: EmbeddingCache,
    /// Per-repo caches (meta + blame), invalidated as one group.
    repo_caches: RepoCaches,
    /// One ancestry cache for every repo (keys hold full SHAs).
    ancestry: Arc<AncestryCache>,
    /// Counts `find_pattern` calls that needed the ancestry filter.
    ancestry_filtered_count: AtomicU64,
    /// Perf-harness switch: when set, `find_pattern` skips visibility.
    visibility_bypass: AtomicBool,
    /// Unix-second timestamp of the last dispatched request.
    /// Written by [`Self::touch`]; read by [`Self::idle_for`].
    last_request_at: AtomicU64,
    /// Plan-33 daemon-side indexing; serialises jobs per repository.
    index_service: IndexService,
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
            stores: RwLock::new(HashMap::new()),
            repos: RwLock::new(HashMap::new()),
            embed_cache: EmbeddingCache::new(model_id, 256),
            repo_caches: RepoCaches::new(),
            ancestry: Arc::new(AncestryCache::default()),
            ancestry_filtered_count: AtomicU64::new(0),
            visibility_bypass: AtomicBool::new(false),
            last_request_at: AtomicU64::new(now_unix),
            index_service: IndexService::default(),
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
    /// Zero while an index or prune job runs, so the idle watchdog never
    /// exits mid-job. Saturates at zero if the clock went backwards.
    pub fn idle_for(&self) -> Duration {
        if self.index_service.is_busy() {
            return Duration::ZERO;
        }
        let last = self.last_request_at.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(last);
        Duration::from_secs(now.saturating_sub(last))
    }

    /// Whether an index or prune job is running.
    pub(crate) fn has_running_job(&self) -> bool {
        self.index_service.is_busy()
    }

    /// The idle watchdog's decision: `true` when the engine has been idle
    /// for at least `idle`, in which case the daemon shuts down. Decided
    /// under the job admission lock; once `true`, every index or prune job
    /// that tries to start is refused, so none starts between the decision
    /// and the stop.
    pub fn begin_idle_shutdown(&self, idle: Duration) -> bool {
        self.index_service
            .decide_shutdown(|| self.idle_for() >= idle)
    }

    /// Returns the number of times `find_pattern` served `ResponseMeta`
    /// from the in-memory [`MetaCache`] (i.e. skipped storage).
    ///
    /// Exposed only under `#[cfg(test)]` so unit tests can assert caching
    /// behaviour without touching internal fields.
    #[cfg(test)]
    pub fn meta_hits(&self) -> u64 {
        self.repo_caches.meta.hits()
    }

    /// Returns the number of times `explain_change` served blame ranges
    /// from the in-memory [`BlameCache`] (i.e., skipped `Blamer::blame_range`).
    ///
    /// Exposed only under `#[cfg(test)]` so unit tests can assert caching
    /// behaviour without touching internal fields.
    #[cfg(test)]
    pub fn blame_cache_hits(&self) -> u64 {
        self.repo_caches.blame.hits()
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

    /// Serve `ResponseMeta` from [`MetaCache`] when a fresh entry exists
    /// (TTL = 5 s). On a miss the meta is computed via `compose_response_meta`
    /// and stored before returning.
    async fn cached_meta(&self, handle: &RepoHandle) -> crate::Result<ResponseMeta> {
        if let Some(cached) = self.repo_caches.meta.get(&handle.repo_id, &handle.worktree) {
            return Ok(cached);
        }
        let fresh = compose_response_meta(handle).await?;
        self.repo_caches.meta.put(
            handle.repo_id.clone(),
            handle.worktree.clone(),
            fresh.clone(),
        );
        Ok(fresh)
    }

    /// Semantic search over a repo's indexed git history.
    ///
    /// Opens (or reuses) the per-repo handle, then delegates to the
    /// retriever's three-lane pipeline (vector KNN + BM25 text + BM25
    /// symbol → RRF → optional cross-encoder rerank → recency multiplier).
    ///
    /// `ResponseMeta` is computed BEFORE retrieval so that a
    /// [`CompatibilityStatus::NeedsRebuild`] index is refused before
    /// touching stale vectors (plan-29 guard). This covers the daemon,
    /// CLI, and MCP-fallback paths with one check. Meta is cached via
    /// [`Self::cached_meta`] (TTL = 5 s).
    pub async fn find_pattern(
        &self,
        repo_path: impl AsRef<Path>,
        query: PatternQuery,
    ) -> crate::Result<FindPatternResult> {
        // An unborn HEAD has no history to search (and no repo identity yet).
        let Some(head) = visibility::read_head(repo_path.as_ref()).await? else {
            return Ok(FindPatternResult {
                hits: Vec::new(),
                meta: ResponseMeta {
                    hint: Some("HEAD has no commits yet; nothing to search".into()),
                    ..ResponseMeta::default()
                },
            });
        };
        let handle = self.open_repo(repo_path).await?;
        let meta = self.cached_meta(&handle).await?;
        // Plan-29: refuse before touching the vector index. KNN against
        // stale vectors silently returns wrong results; surfacing
        // NeedsRebuild here covers the daemon, CLI, and MCP-fallback
        // paths with one guard (previously the MCP tool checked locally).
        if let Some(CompatibilityStatus::NeedsRebuild { reason }) = &meta.compatibility {
            return Err(EngineError::NeedsRebuild {
                reason: reason.clone(),
            });
        }
        let now_unix = chrono::Utc::now().timestamp();
        let hits = self
            .retrieve_visible(&handle, &head, &query, now_unix)
            .await?;
        Ok(FindPatternResult { hits, meta })
    }

    /// Freshness + compatibility envelope for a repo, without running a
    /// query. Backs the IPC `IndexStatus` method so thin MCP clients can
    /// build the `_meta` block of `explain_change` responses.
    pub async fn index_status(&self, repo_path: impl AsRef<Path>) -> crate::Result<ResponseMeta> {
        let handle = self.open_repo(repo_path).await?;
        self.cached_meta(&handle).await
    }

    /// Blocks until `sha` is covered by some indexed tip, running one
    /// incremental index of this worktree if needed. `indexed == false`
    /// means it already was. Never indexes a repo from scratch.
    pub async fn ensure_indexed(
        &self,
        repo_path: impl AsRef<Path>,
        sha: &CommitSha,
    ) -> crate::Result<EnsureIndexedOutcome> {
        self.index_service
            .ensure_indexed(self, repo_path.as_ref(), sha)
            .await
    }

    /// Index this worktree's HEAD now (used by the queue worker and by
    /// [`Self::ensure_indexed`]).
    pub async fn index_worktree(
        &self,
        repo_path: impl AsRef<Path>,
    ) -> crate::Result<EnsureIndexedOutcome> {
        self.index_service
            .index_worktree(self, repo_path.as_ref())
            .await
    }

    /// Delete indexed commits no ref or worktree HEAD reaches and the tips
    /// of linked worktrees that no longer exist.
    pub async fn prune_unreachable(
        &self,
        repo_path: impl AsRef<Path>,
    ) -> crate::Result<PruneOutcome> {
        self.index_service
            .prune_unreachable(self, repo_path.as_ref())
            .await
    }

    /// Queue this worktree's HEAD for indexing. Never blocks on indexing.
    /// A worktree already waiting in the queue is not queued twice.
    /// Returns `false` when coalesced with a waiting entry.
    pub fn enqueue_index_tip(&self, repo_path: PathBuf) -> bool {
        self.index_service.enqueue(repo_path)
    }

    /// Drain the queue until `stop` is cancelled. One job at a time; a
    /// failed job is logged once and dropped. A started job runs to
    /// completion — `stop` is only observed between jobs.
    pub async fn run_index_queue(self: Arc<Self>, stop: tokio_util::sync::CancellationToken) {
        self.index_service.run_queue(&self, stop).await
    }

    /// Ask the reranker to drop its session when idle for at least `idle`.
    /// Returns `true` when a session was dropped (plan-29 tiered unload).
    pub async fn unload_idle_reranker(&self, idle: std::time::Duration) -> bool {
        self.reranker.unload_if_idle(idle).await
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
            if let Some(cached) = self.repo_caches.blame.get(
                &handle.repo_id,
                &handle.worktree,
                &query.file,
                hash.as_str(),
            ) {
                tracing::debug!(file = %query.file, "explain_change: BlameCache hit");
                let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
                    ohara_core::explain::hydrator::HydrateInputs {
                        storage: &*handle.store.storage,
                        blame_ranges: (*cached).clone(),
                        query: &query,
                        repo_id: &handle.repo_id,
                        git_fallback: Some(handle.blamer.as_ref()),
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
            self.repo_caches.blame.put(
                handle.repo_id.clone(),
                handle.worktree.clone(),
                query.file.clone(),
                hash.as_str().to_string(),
                Arc::new(raw_ranges.clone()),
            );
        }

        let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
            ohara_core::explain::hydrator::HydrateInputs {
                storage: &*handle.store.storage,
                blame_ranges: raw_ranges,
                query: &query,
                repo_id: &handle.repo_id,
                git_fallback: Some(handle.blamer.as_ref()),
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
/// Freshness is per worktree (plan-33): `last_indexed_commit` is this
/// worktree's tip (else the main tip) and `commits_behind_head` counts
/// commits reachable from this worktree's HEAD but from no indexed tip.
///
/// Called on a MetaCache miss inside [`RetrievalEngine::find_pattern`].
///
/// Issue #40: query-time callers don't know the user's `--embed-cache`
/// intent (no flag at query time), so we adopt the stored
/// `embed_input_mode` for the runtime expectation when present. An
/// internally-consistent `--embed-cache=diff` index then assesses as
/// `Compatible`, not `NeedsRebuild`. Mirrors the same override in
/// `ohara-cli`'s `status` command.
pub(crate) async fn compose_response_meta(handle: &RepoHandle) -> crate::Result<ResponseMeta> {
    let behind = ohara_git::GitCommitsBehind::open(&handle.repo_path)
        .map_err(|e| EngineError::Git(format!("commits_behind open: {e}")))?;
    let st = ohara_core::query::compute_worktree_index_status(
        handle.store.storage.as_ref(),
        handle.store.tips.as_ref(),
        &handle.repo_id,
        &handle.worktree,
        &behind,
    )
    .await
    .map_err(EngineError::from)?;
    let stored = handle
        .store
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
