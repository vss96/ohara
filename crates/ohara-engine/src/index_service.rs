//! Daemon-side indexing (plan-33): incrementally index one worktree into
//! the repository's shared store, make sure a commit is covered by some
//! indexed tip, and prune commits that no ref or worktree HEAD reaches.

use crate::cache::CachedVisibility;
use crate::engine::visibility::read_head;
use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::index_metadata::{
    runtime_metadata_from, CompatibilityStatus, RuntimeIndexMetadata,
};
use ohara_core::types::RepoId;
use ohara_core::{CommitSha, CommitVisibility, EmbedMode, IndexedTip, Indexer, TipContext};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Commits deleted per `TipStore::delete_commits` call (one transaction each).
const PRUNE_BATCH: usize = 500;

/// Result of [`RetrievalEngine::ensure_indexed`] and
/// [`RetrievalEngine::index_worktree`].
#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureIndexedOutcome {
    /// `true` when an index pass ran; `false` when the commit was already covered.
    pub indexed: bool,
    /// Commits written by the pass (0 when nothing ran).
    pub new_commits: u64,
    /// Wall time of the whole call in milliseconds.
    pub elapsed_ms: u64,
}

/// Result of [`RetrievalEngine::prune_unreachable`].
#[derive(Debug, Serialize, Deserialize)]
pub struct PruneOutcome {
    /// Commits removed from the index because nothing reaches them any more.
    pub commits_deleted: u64,
    /// Linked-worktree tips removed because their worktree no longer exists.
    pub tips_removed: u64,
}

/// Serialises index and prune jobs per repository and owns the background
/// index queue. Owned by [`RetrievalEngine`].
#[derive(Default)]
pub(crate) struct IndexService {
    locks: std::sync::Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
    /// Index / prune jobs currently running (any repo).
    active: AtomicUsize,
    queue: IndexQueue,
}

/// Worktree paths waiting for a background index pass. A path is in
/// `pending` from enqueue until its job *starts*, so a commit made while
/// the job runs queues the worktree again.
struct IndexQueue {
    tx: mpsc::UnboundedSender<PathBuf>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<PathBuf>>,
    pending: std::sync::Mutex<HashSet<PathBuf>>,
}

impl Default for IndexQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
            pending: std::sync::Mutex::new(HashSet::new()),
        }
    }
}

impl IndexQueue {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        // The set is only ever inserted into / removed from; a panic while
        // holding it cannot leave it inconsistent.
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue `path` unless it is already waiting. `false` when coalesced.
    fn push(&self, path: PathBuf) -> bool {
        // Canonicalise so `/a/../a` and `/a` coalesce; a path that cannot
        // be resolved is queued as given and fails in its job.
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !self.pending().insert(path.clone()) {
            return false;
        }
        if let Err(e) = self.tx.send(path) {
            // Unreachable while `self` owns the receiver; undo the mark.
            self.pending().remove(&e.0);
            tracing::warn!(worktree = %e.0.display(), "index queue closed");
            return false;
        }
        true
    }
}

/// Marks a job as running so the idle watchdog never sees the daemon as
/// idle mid-job; touches the engine's activity clock at start and end.
struct JobGuard<'a> {
    active: &'a AtomicUsize,
    engine: &'a RetrievalEngine,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.engine.touch();
    }
}

impl IndexService {
    fn begin_job<'a>(&'a self, engine: &'a RetrievalEngine) -> JobGuard<'a> {
        self.active.fetch_add(1, Ordering::SeqCst);
        engine.touch();
        JobGuard {
            active: &self.active,
            engine,
        }
    }

    /// Whether any index or prune job is running.
    pub(crate) fn is_busy(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }

    /// See [`RetrievalEngine::enqueue_index_tip`].
    pub(crate) fn enqueue(&self, repo_path: PathBuf) -> bool {
        self.queue.push(repo_path)
    }

    /// See [`RetrievalEngine::run_index_queue`]. A job that has started
    /// always runs to completion; `stop` is only observed between jobs.
    pub(crate) async fn run_queue(&self, engine: &RetrievalEngine, stop: CancellationToken) {
        let mut rx = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            rx = self.queue.rx.lock() => rx,
        };
        loop {
            let path = tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                next = rx.recv() => match next {
                    Some(p) => p,
                    None => return,
                },
            };
            self.queue.pending().remove(&path);
            if let Err(e) = self.index_worktree(engine, &path).await {
                tracing::warn!(worktree = %path.display(), error = %e, "background index job failed");
            }
        }
    }

    /// The job lock for `repo_id`, created on first use.
    fn lock_for(&self, repo_id: &RepoId) -> crate::Result<Arc<tokio::sync::Mutex<()>>> {
        let mut map = self
            .locks
            .lock()
            .map_err(|_| EngineError::Internal("index lock map poisoned".into()))?;
        Ok(map.entry(repo_id.clone()).or_default().clone())
    }

    /// Blocks until `sha` is covered by some indexed tip.
    pub(crate) async fn ensure_indexed(
        &self,
        engine: &RetrievalEngine,
        repo_path: &Path,
        sha: &CommitSha,
    ) -> crate::Result<EnsureIndexedOutcome> {
        let start = Instant::now();
        let _busy = self.begin_job(engine);
        let handle = engine.open_repo(repo_path).await?;
        if covered(engine, &handle, sha).await? {
            return Ok(outcome(false, 0, start));
        }
        let lock = self.lock_for(&handle.repo_id)?;
        let _job = lock.lock().await;
        // Another job may have covered `sha` while we waited.
        if covered(engine, &handle, sha).await? {
            return Ok(outcome(false, 0, start));
        }
        let new_commits = index_locked(engine, &handle).await?;
        if !covered(engine, &handle, sha).await? {
            return Err(EngineError::Internal(format!(
                "commit {} is not reachable from this worktree's HEAD",
                sha.as_str()
            )));
        }
        Ok(outcome(true, new_commits, start))
    }

    /// Index the worktree's HEAD now.
    pub(crate) async fn index_worktree(
        &self,
        engine: &RetrievalEngine,
        repo_path: &Path,
    ) -> crate::Result<EnsureIndexedOutcome> {
        let start = Instant::now();
        let _busy = self.begin_job(engine);
        let handle = engine.open_repo(repo_path).await?;
        let lock = self.lock_for(&handle.repo_id)?;
        let _job = lock.lock().await;
        let new_commits = index_locked(engine, &handle).await?;
        Ok(outcome(true, new_commits, start))
    }

    /// Delete indexed commits no ref or worktree HEAD reaches, then drop
    /// tips of linked worktrees that no longer exist. The main tip stays.
    pub(crate) async fn prune_unreachable(
        &self,
        engine: &RetrievalEngine,
        repo_path: &Path,
    ) -> crate::Result<PruneOutcome> {
        let _busy = self.begin_job(engine);
        let handle = engine.open_repo(repo_path).await?;
        let lock = self.lock_for(&handle.repo_id)?;
        let _job = lock.lock().await;
        let result = prune_locked(&handle).await;
        // Refresh even on failure: earlier delete batches are already committed.
        engine.refresh_repo_caches(&handle.repo_id);
        let (commits_deleted, tips_removed) = result?;
        tracing::info!(commits_deleted, tips_removed, "pruned unreachable commits");
        Ok(PruneOutcome {
            commits_deleted,
            tips_removed,
        })
    }
}

/// Prune body; returns `(commits_deleted, tips_removed)`. Caller holds
/// the repo's job lock and refreshes caches whatever the result.
async fn prune_locked(handle: &RepoHandle) -> crate::Result<(u64, u64)> {
    let tips = &handle.store.tips;
    // List indexed commits BEFORE snapshotting reachability. Another process
    // (e.g. the CLI post-commit hook) may index a new commit meanwhile; with
    // this order that commit is never a candidate. The reverse order would
    // see it indexed but missing from a stale reachable set, and delete it.
    let indexed = tips
        .indexed_commit_shas(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    let (reachable, live_worktrees) = git_liveness(handle.repo_path.clone()).await?;
    let doomed: Vec<CommitSha> = indexed
        .into_iter()
        .filter(|c| !reachable.contains(c.as_str()))
        .collect();
    let mut commits_deleted = 0u64;
    for batch in doomed.chunks(PRUNE_BATCH) {
        commits_deleted += tips
            .delete_commits(&handle.repo_id, batch)
            .await
            .map_err(EngineError::from)?;
    }
    let stale: Vec<IndexedTip> = tips
        .indexed_tips(&handle.repo_id)
        .await
        .map_err(EngineError::from)?
        .into_iter()
        .filter(|t| !t.worktree.is_main() && !live_worktrees.contains(t.worktree.as_str()))
        .collect();
    for tip in &stale {
        tips.remove_indexed_tip(&handle.repo_id, &tip.worktree)
            .await
            .map_err(EngineError::from)?;
    }
    Ok((commits_deleted, stale.len() as u64))
}

/// Metadata describing the engine's live embedder (model id, dimension)
/// plus the current chunker / parser / reranker versions.
fn live_runtime_metadata(engine: &RetrievalEngine, mode: EmbedMode) -> RuntimeIndexMetadata {
    let embedder = engine.embedder();
    runtime_metadata_from(
        embedder.model_id(),
        u32::try_from(embedder.dimension()).unwrap_or(u32::MAX),
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
        mode.index_metadata_value(),
    )
}

fn outcome(indexed: bool, new_commits: u64, start: Instant) -> EnsureIndexedOutcome {
    EnsureIndexedOutcome {
        indexed,
        new_commits,
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

/// Whether some indexed tip `t` has `visible(t, sha)`. No tips at all is
/// [`EngineError::NoIndex`]: the daemon never indexes from scratch.
async fn covered(
    engine: &RetrievalEngine,
    handle: &RepoHandle,
    sha: &CommitSha,
) -> crate::Result<bool> {
    let tips = handle
        .store
        .tips
        .indexed_tips(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    if tips.is_empty() {
        return Err(no_index(handle));
    }
    let git = ohara_git::GitVisibility::open(&handle.repo_path)
        .map_err(|e| EngineError::Git(format!("open visibility oracle: {e:#}")))?;
    let oracle = CachedVisibility::new(Arc::new(git), engine.ancestry_cache());
    let target = std::slice::from_ref(sha);
    for tip in &tips {
        // A tip whose commit is gone (e.g. pruned) cannot cover anything.
        match oracle.visible(&tip.sha, target).await {
            Ok(v) if v.first().copied().unwrap_or(false) => return Ok(true),
            Ok(_) => {}
            Err(e) => tracing::debug!(tip = tip.sha.as_str(), error = %e, "tip not usable"),
        }
    }
    Ok(false)
}

fn no_index(handle: &RepoHandle) -> EngineError {
    EngineError::NoIndex {
        path: handle.repo_path.display().to_string(),
    }
}

/// One incremental pass over `handle`'s worktree. Caller holds the repo's
/// job lock. Refuses unless the stored index is fully `Compatible` so
/// mismatched vectors are never appended.
async fn index_locked(engine: &RetrievalEngine, handle: &RepoHandle) -> crate::Result<u64> {
    let store = &handle.store;
    let has_tips = !store
        .tips
        .indexed_tips(&handle.repo_id)
        .await
        .map_err(EngineError::from)?
        .is_empty();
    if !has_tips {
        return Err(no_index(handle));
    }
    let meta = crate::engine::compose_response_meta(handle).await?;
    match meta.compatibility {
        Some(CompatibilityStatus::Compatible) => {}
        other => {
            return Err(EngineError::NeedsRebuild {
                reason: format!("daemon indexing requires a compatible index, got {other:?}"),
            })
        }
    }
    let stored = store
        .storage
        .get_index_metadata(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    let mode = match stored
        .components
        .get("embed_input_mode")
        .map(String::as_str)
    {
        Some("diff") => EmbedMode::Diff,
        _ => EmbedMode::Semantic,
    };
    // Review Focus 5: the live embedder (not just the binary's defaults)
    // must match the stored model class and dimension, or its vectors
    // would be appended to an index built with another model.
    let runtime = live_runtime_metadata(engine, mode);
    match CompatibilityStatus::assess(&runtime, &stored) {
        CompatibilityStatus::Compatible => {}
        other => {
            return Err(EngineError::NeedsRebuild {
                reason: format!("engine embedder does not match the index: {other:?}"),
            })
        }
    }
    let head = read_head(&handle.repo_path).await?;
    let commit_source = Arc::new(
        ohara_git::GitCommitSource::open(&handle.repo_path)
            .map_err(|e| EngineError::Git(format!("open commit source: {e:#}")))?,
    );
    let symbol_source = Arc::new(
        ohara_parse::GitSymbolSource::open(&handle.repo_path)
            .map_err(|e| EngineError::Git(format!("open symbol source: {e:#}")))?,
    );
    let indexer = Indexer::new(store.storage.clone(), engine.embedder())
        .with_runtime_metadata(runtime)
        .with_atomic_symbol_extractor(Arc::new(ohara_parse::TreeSitterAtomicExtractor))
        .with_repo_root(handle.repo_path.clone())
        .with_embed_mode(mode)
        .with_tip_context(TipContext {
            tips: store.tips.clone(),
            worktree: handle.worktree.clone(),
            head,
        });
    let report = indexer
        .run(&handle.repo_id, commit_source, symbol_source)
        .await;
    // Refresh caches even when the run failed part-way: rows may have landed.
    engine.refresh_repo_caches(&handle.repo_id);
    let report = report.map_err(EngineError::from)?;
    if report.commits_failed > 0 {
        return Err(EngineError::Internal(format!(
            "{} commits failed to index; tip not advanced",
            report.commits_failed
        )));
    }
    tracing::info!(
        worktree = handle.worktree.as_str(),
        new_commits = report.new_commits,
        "indexed worktree"
    );
    Ok(report.new_commits as u64)
}

/// Commits reachable from any ref or worktree HEAD, and the names of the
/// linked worktrees git still knows about. Fails closed on any git error.
async fn git_liveness(path: PathBuf) -> crate::Result<(HashSet<String>, HashSet<String>)> {
    tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let reachable = ohara_git::reachable_commit_shas(&path)
            .map_err(|e| EngineError::Git(format!("reachable commits: {e:#}")))?;
        let repo = git2::Repository::discover(&path)
            .map_err(|e| EngineError::Git(format!("open {}: {e}", path.display())))?;
        let names = repo
            .worktrees()
            .map_err(|e| EngineError::Git(format!("list worktrees: {e}")))?;
        let live = names.iter().flatten().map(str::to_string).collect();
        Ok((reachable, live))
    })
    .await
    .map_err(|e| EngineError::Internal(format!("prune git task: {e}")))?
}
