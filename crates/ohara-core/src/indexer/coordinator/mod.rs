//! Coordinator: drives the 5-stage pipeline per commit.

use num_cpus;

use crate::indexer::stages::attribute::AttributedHunk;
use crate::indexer::stages::commit_walk::CommitWatermark;
use crate::indexer::stages::{
    attribute::AttributeStage, commit_walk::CommitWalkStage, embed::EmbedStage,
    hunk_chunk::HunkChunkStage, persist::PersistStage,
};
use crate::indexer::{
    AtomicSymbolExtractor, CommitSource, NullAtomicSymbolExtractor, NullProgress, PhaseTimings,
    ProgressSink, SymbolSource,
};
use crate::types::{CommitMeta, RepoId};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;
use std::time::Instant;

/// Liveness signal — emit an info-level event every N commits so
/// `RUST_LOG=info` users without a visible progress bar still see
/// steady forward motion. Also bounds resume-time worst-case to
/// `PROGRESS_INTERVAL` re-walked commits after an abort. Issue #29 —
/// at 100 a 5k-commit first-time run produced one tick every ~30s; 25
/// gives ~one every few seconds without flooding.
///
/// Used by the actor pipeline via the shared atomic counter sent to workers.
const PROGRESS_INTERVAL: usize = 25;

/// Counts and phase timings returned by `Coordinator::run_timed`.
/// Used by `Indexer::run` to populate `IndexerReport` after delegating
/// the per-commit pipeline to the coordinator.
#[derive(Debug, Default)]
pub struct CoordinatorResult {
    /// Number of commits that were newly indexed (not already in storage).
    pub new_commits: usize,
    /// Total hunk count across all newly indexed commits.
    pub new_hunks: usize,
    /// Total diff bytes summed across newly indexed hunks.
    pub total_diff_bytes: u64,
    /// Total added lines summed across newly indexed hunks.
    pub total_added_lines: u64,
    /// Accumulated phase timings measured by the coordinator.
    pub timings: PhaseTimings,
    /// SHA of the last commit processed (for watermark advance).
    pub latest_sha: Option<String>,
}

/// Per-commit result returned by `run_commit_owned`.
#[derive(Default)]
struct CommitWorkResult {
    new_hunks: usize,
    total_diff_bytes: u64,
    total_added_lines: u64,
    /// True when the commit was fully persisted (not 100%-ignored).
    persisted: bool,
    /// Per-commit timing contributions.
    diff_extract_ms: u64,
    embed_ms: u64,
    storage_write_ms: u64,
}

/// Aggregate per-worker results.
#[derive(Default)]
struct WorkerResult {
    local: CommitWorkResult,
    succeeded: usize,
    /// Last error seen in this worker, if any.
    last_error: Option<crate::OhraError>,
}

/// Drives the 5-stage indexer pipeline per commit.
///
/// The coordinator:
/// - Queries `Storage::get_index_status` once per run to build the
///   resume watermark.
/// - Filters `CommitWalkStage` output to skip already-indexed commits.
/// - Orchestrates stages 2-5 per commit.
/// - Does NOT hold per-stage state — stages are constructed fresh per
///   `run` call so the coordinator is safe to re-use across runs.
pub struct Coordinator {
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    progress: Arc<dyn ProgressSink>,
    ignore_filter: Option<Arc<dyn crate::IgnoreFilter>>,
    embed_mode: crate::EmbedMode,
    /// Plan 27: storage handle reused by EmbedStage for the
    /// chunk-embed cache. Set when embed_mode != Off.
    cache_storage: Option<Arc<dyn crate::Storage>>,
    workers: usize,
}

impl Coordinator {
    /// Construct a coordinator with the default `embed_batch` of 32
    /// and a no-op progress sink.
    pub fn new(
        storage: Arc<dyn Storage + Send + Sync>,
        embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    ) -> Self {
        Self {
            storage,
            embedder,
            embed_batch: 32,
            progress: Arc::new(NullProgress),
            ignore_filter: None,
            embed_mode: crate::EmbedMode::default(),
            cache_storage: None,
            workers: num_cpus::get().max(1),
        }
    }

    /// Override the embed stage's batch size.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Wire a [`ProgressSink`] so the coordinator can drive `pre_walk`,
    /// `start`, and per-commit `commit_done` updates from inside the
    /// pipeline. Defaults to [`NullProgress`].
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Wire a [`crate::ignore::LayeredIgnore`] (or any `IgnoreFilter`
    /// impl). When set, the per-commit pipeline drops `HunkRecord`s
    /// whose path matches the filter, and skips a commit entirely when
    /// 100% of its changed paths matched (advancing the watermark in
    /// either case). Plumbing-only in C.1; behaviour added in C.2.
    pub fn with_ignore_filter(mut self, f: Arc<dyn crate::IgnoreFilter>) -> Self {
        self.ignore_filter = Some(f);
        self
    }

    /// Plan 28: set the number of worker tasks. `n.max(1)` is enforced.
    pub fn with_workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }

    /// Plan 27: set the chunk-embed cache mode. When mode != Off, the
    /// existing `storage` handle is reused as the cache backend.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        if mode != crate::EmbedMode::Off {
            // Reuse the storage handle that was passed to `new`.
            self.cache_storage = Some(self.storage.clone());
        }
        self
    }

    /// Run the full 5-stage pipeline for all commits in `source` that
    /// follow the resume watermark.
    ///
    /// Returns `()` — use `run_timed` when `IndexerReport` fields are needed.
    pub async fn run(
        &self,
        repo: &RepoId,
        commit_source: Arc<dyn CommitSource>,
        symbol_source: Arc<dyn SymbolSource>,
    ) -> Result<()> {
        self.run_timed_with_extractor(
            repo,
            commit_source,
            symbol_source,
            Arc::new(NullAtomicSymbolExtractor),
        )
        .await?;
        Ok(())
    }

    /// Run the 5-stage pipeline and return phase timing + counts for
    /// report hydration by `Indexer::run`.
    ///
    /// `symbol_extractor` is used by the attribute stage for per-hunk
    /// ExactSpan attribution. Pass `NullAtomicSymbolExtractor` when no
    /// tree-sitter extractor is wired.
    pub async fn run_timed(
        &self,
        repo: &RepoId,
        commit_source: Arc<dyn CommitSource>,
        symbol_source: Arc<dyn SymbolSource>,
    ) -> Result<CoordinatorResult> {
        self.run_timed_with_extractor(
            repo,
            commit_source,
            symbol_source,
            Arc::new(NullAtomicSymbolExtractor),
        )
        .await
    }

    /// Like `run_timed` but accepts a custom `AtomicSymbolExtractor`
    /// for ExactSpan attribution. Used by `Indexer::run` to pass the
    /// configured `symbol_extractor` through to the attribute stage.
    ///
    /// Accepts `Arc<dyn ...>` so that the trait objects can be moved
    /// into spawned tokio tasks (plan-28 actor pipeline).
    pub async fn run_timed_with_extractor(
        &self,
        repo: &RepoId,
        commit_source: Arc<dyn CommitSource>,
        symbol_source: Arc<dyn SymbolSource>,
        extractor: Arc<dyn AtomicSymbolExtractor>,
    ) -> Result<CoordinatorResult> {
        let mut result = CoordinatorResult::default();

        // Stage 0: determine resume watermark from index status.
        let status = self.storage.get_index_status(repo).await?;
        let watermark = status
            .last_indexed_commit
            .as_deref()
            .map(CommitWatermark::new);

        // Stage 1: commit walk (timed). The walk is silent and can take
        // seconds on multi-thousand-commit repos, so spin a pre-walk
        // indicator and emit an info log so users without a TTY-rendered
        // bar still see motion (issue #29).
        self.progress.pre_walk("walking commit history");
        tracing::info!("walking commit history");
        let walk_start = Instant::now();
        let commits = CommitWalkStage::run(commit_source.as_ref(), watermark.as_ref()).await?;
        result.timings.commit_walk_ms = walk_start.elapsed().as_millis() as u64;
        tracing::info!(
            new_commits = commits.len(),
            walk_ms = result.timings.commit_walk_ms,
            "commit walk complete; begin per-commit indexing"
        );
        self.progress.start(commits.len());

        // Capture the last commit SHA before splitting commits across the
        // actor tasks. This covers both the "all already indexed" and the
        // "some ignored" cases where a worker's `CommitWorkResult::sha`
        // might not reach the true last commit in the walk list.
        let walk_last_sha: Option<String> = commits.last().map(|c| c.commit_sha.clone());

        // Plan 28: actor-style pipeline — one walker task feeds N worker
        // tasks through a bounded mpsc channel. The ULID is computed by
        // `PersistStage` internally (with its own short-SHA guard), so the
        // walker does not need to produce it.
        let n_workers = self.workers.max(1);
        let (tx, rx) = tokio::sync::mpsc::channel::<CommitMeta>(n_workers);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        // Walker task: filter already-indexed commits and send to workers.
        let storage_for_walker = self.storage.clone();
        let walker_handle = tokio::spawn(async move {
            for commit in commits {
                if storage_for_walker
                    .commit_exists(&commit.commit_sha)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                if tx.send(commit).await.is_err() {
                    break;
                }
            }
        });

        // Shared atomic counters for progress reporting across workers.
        let commits_done_atomic = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let new_hunks_atomic = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Worker tasks: pull (CommitMeta, Ulid) pairs and run the per-commit pipeline.
        let mut worker_handles = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let rx_for_worker = rx.clone();
            let storage = self.storage.clone();
            let embedder = self.embedder.clone();
            let embed_batch = self.embed_batch;
            let embed_mode = self.embed_mode;
            let cache_storage = self.cache_storage.clone();
            let ignore_filter = self.ignore_filter.clone();
            let commit_source_arc = commit_source.clone();
            let symbol_source_arc = symbol_source.clone();
            let extractor_arc = extractor.clone();
            let repo_owned = repo.clone();
            let progress_arc = self.progress.clone();
            let commits_done_ref = commits_done_atomic.clone();
            let new_hunks_ref = new_hunks_atomic.clone();
            worker_handles.push(tokio::spawn(async move {
                let mut wr = WorkerResult::default();
                loop {
                    let next = {
                        let mut guard = rx_for_worker.lock().await;
                        guard.recv().await
                    };
                    let Some(commit) = next else { break };
                    match run_commit_owned(
                        storage.clone(),
                        embedder.clone(),
                        embed_batch,
                        embed_mode,
                        cache_storage.clone(),
                        ignore_filter.clone(),
                        repo_owned.clone(),
                        commit,
                        commit_source_arc.clone(),
                        symbol_source_arc.clone(),
                        extractor_arc.clone(),
                    )
                    .await
                    {
                        Ok(r) => {
                            wr.local.new_hunks += r.new_hunks;
                            wr.local.total_diff_bytes += r.total_diff_bytes;
                            wr.local.total_added_lines += r.total_added_lines;
                            wr.local.diff_extract_ms += r.diff_extract_ms;
                            wr.local.embed_ms += r.embed_ms;
                            wr.local.storage_write_ms += r.storage_write_ms;
                            if r.persisted {
                                wr.succeeded += 1;
                            }
                            let done = commits_done_ref
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                + 1;
                            let hunks = new_hunks_ref
                                .fetch_add(r.new_hunks, std::sync::atomic::Ordering::Relaxed)
                                + r.new_hunks;
                            progress_arc.commit_done(done, hunks);
                            if done % PROGRESS_INTERVAL == 0 {
                                tracing::info!(
                                    commits_done = done,
                                    new_hunks = hunks,
                                    "indexing progress"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "plan-28 worker error; commit skipped");
                            wr.last_error = Some(e);
                        }
                    }
                }
                wr
            }));
        }

        let _ = walker_handle.await;
        let mut last_worker_error: Option<crate::OhraError> = None;
        for handle in worker_handles {
            if let Ok(wr) = handle.await {
                result.new_commits += wr.succeeded;
                result.new_hunks += wr.local.new_hunks;
                result.total_diff_bytes += wr.local.total_diff_bytes;
                result.total_added_lines += wr.local.total_added_lines;
                result.timings.diff_extract_ms += wr.local.diff_extract_ms;
                result.timings.embed_ms += wr.local.embed_ms;
                result.timings.storage_write_ms += wr.local.storage_write_ms;
                if wr.last_error.is_some() {
                    last_worker_error = wr.last_error;
                }
            }
        }
        // Re-propagate the last worker error so callers can detect
        // systemic failures (e.g. a broken embedder that fails every
        // commit). Per-commit isolation still applies: partial progress
        // is accumulated before the error is returned.
        if let Some(err) = last_worker_error {
            return Err(err);
        }

        // The watermark must advance to the last commit in the walk list,
        // not just the last one a worker happened to process. Parallel
        // workers process commits out-of-order, so using a worker's last
        // SHA would give a non-deterministic and potentially stale
        // watermark. `walk_last_sha` is the true end of the walk and
        // matches the serial-loop behaviour that set `latest_sha` for
        // every commit regardless of skip/process status.
        result.latest_sha = walk_last_sha;

        Ok(result)
    }

    /// Run stages 2-5 for a single commit, accumulating timing and counts.
    ///
    /// Kept for the `run_from_attributed` path and future fallback use.
    /// The actor pipeline in `run_timed_with_extractor` uses
    /// `run_commit_owned` instead.
    #[allow(dead_code)]
    async fn run_commit_timed(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        extractor: &dyn AtomicSymbolExtractor,
        result: &mut CoordinatorResult,
    ) -> Result<()> {
        // Stage 2: hunk chunk (timed).
        let extract_start = Instant::now();
        let mut records = HunkChunkStage::run(commit_source, commit).await?;
        result.timings.diff_extract_ms += extract_start.elapsed().as_millis() as u64;

        // Plan 26 Task C.2: drop ignored paths before downstream stages.
        let paths_total = records.len();
        if let Some(filter) = self.ignore_filter.as_ref() {
            records.retain(|r| !filter.is_ignored(&r.file_path));
        }
        let paths_kept = records.len();
        if paths_total > 0 && paths_kept == 0 {
            tracing::debug!(
                sha = %commit.commit_sha,
                "plan-26: commit has 100% ignored paths; skipping (watermark advances)"
            );
            return Ok(());
        }

        // Accumulate diff metrics for inflation-ratio diagnostic.
        for rec in &records {
            result.total_diff_bytes += rec.diff_text.len() as u64;
            result.total_added_lines += count_added_lines_stage(&rec.diff_text);
        }
        result.new_hunks += records.len();

        // Stage 3: attribute.
        let attributed = AttributeStage::run(
            &records,
            &commit.commit_sha,
            commit_source,
            symbol_source,
            extractor,
        )
        .await?;

        // Stage 4: embed.
        let mut embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch)
            .with_embed_mode(self.embed_mode);
        if let Some(cache) = self.cache_storage.as_ref() {
            embed_stage = embed_stage.with_cache(cache.clone());
        }
        let embed_start = Instant::now();
        let embed_output = embed_stage.run(&commit.message, &attributed).await?;
        result.timings.embed_ms += embed_start.elapsed().as_millis() as u64;

        // Stage 5: persist.
        let write_start = Instant::now();
        PersistStage::run(repo, commit, embed_output, self.storage.as_ref()).await?;
        result.timings.storage_write_ms += write_start.elapsed().as_millis() as u64;

        result.new_commits += 1;
        Ok(())
    }

    /// Run stages 4 (embed) and 5 (persist) given pre-built
    /// `AttributedHunk` values.
    ///
    /// This entry point enables "resume from after attribute stage":
    /// a caller can construct `Vec<AttributedHunk>` directly (e.g.
    /// from a checkpoint) and drive only the downstream stages.
    pub async fn run_from_attributed(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        attributed: Vec<AttributedHunk>,
    ) -> Result<()> {
        let embed_stage = EmbedStage::new(self.embedder.clone()).with_embed_batch(self.embed_batch);

        // Stage 4: embed.
        let embed_output = embed_stage.run(&commit.message, &attributed).await?;

        // Stage 5: persist.
        PersistStage::run(repo, commit, embed_output, self.storage.as_ref()).await
    }
}

/// Free async function that runs stages 2-5 for a single commit.
/// Used by the actor worker tasks spawned in `run_timed_with_extractor`.
/// The walker has already done the `commit_exists` check, so it is not
/// repeated here.
#[allow(clippy::too_many_arguments)]
async fn run_commit_owned(
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    embed_mode: crate::EmbedMode,
    cache_storage: Option<Arc<dyn crate::Storage>>,
    ignore_filter: Option<Arc<dyn crate::IgnoreFilter>>,
    repo: RepoId,
    commit: CommitMeta,
    commit_source: Arc<dyn CommitSource>,
    symbol_source: Arc<dyn SymbolSource>,
    extractor: Arc<dyn AtomicSymbolExtractor>,
) -> Result<CommitWorkResult> {
    // Stage 2: hunk chunk (timed).
    let extract_start = Instant::now();
    let mut records = HunkChunkStage::run(commit_source.as_ref(), &commit).await?;
    let diff_extract_ms = extract_start.elapsed().as_millis() as u64;

    // Plan 26: drop ignored paths before downstream stages.
    let paths_total = records.len();
    if let Some(filter) = ignore_filter.as_ref() {
        records.retain(|r| !filter.is_ignored(&r.file_path));
    }
    let paths_kept = records.len();
    if paths_total > 0 && paths_kept == 0 {
        tracing::debug!(
            sha = %commit.commit_sha,
            "plan-26: commit has 100% ignored paths; skipping (watermark advances)"
        );
        // Return an empty result — watermark advance happens in the caller
        // via walk_last_sha; succeeded count is not incremented so
        // result.new_commits stays correct.
        return Ok(CommitWorkResult::default());
    }

    let mut new_hunks = 0usize;
    let mut total_diff_bytes = 0u64;
    let mut total_added_lines = 0u64;

    for rec in &records {
        total_diff_bytes += rec.diff_text.len() as u64;
        total_added_lines += count_added_lines_stage(&rec.diff_text);
    }
    new_hunks += records.len();

    // Stage 3: attribute.
    let attributed = AttributeStage::run(
        &records,
        &commit.commit_sha,
        commit_source.as_ref(),
        symbol_source.as_ref(),
        extractor.as_ref(),
    )
    .await?;

    // Stage 4: embed (timed).
    let mut embed_stage = EmbedStage::new(embedder)
        .with_embed_batch(embed_batch)
        .with_embed_mode(embed_mode);
    if let Some(cache) = cache_storage.as_ref() {
        embed_stage = embed_stage.with_cache(cache.clone());
    }
    let embed_start = Instant::now();
    let embed_output = embed_stage.run(&commit.message, &attributed).await?;
    let embed_ms = embed_start.elapsed().as_millis() as u64;

    // Stage 5: persist (timed).
    let write_start = Instant::now();
    PersistStage::run(&repo, &commit, embed_output, storage.as_ref()).await?;
    let storage_write_ms = write_start.elapsed().as_millis() as u64;

    Ok(CommitWorkResult {
        new_hunks,
        total_diff_bytes,
        total_added_lines,
        persisted: true,
        diff_extract_ms,
        embed_ms,
        storage_write_ms,
    })
}

/// Count "+"-prefixed lines in a unified-diff snippet (excludes `+++` headers).
fn count_added_lines_stage(diff_text: &str) -> u64 {
    diff_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count() as u64
}

#[cfg(test)]
mod tests;
