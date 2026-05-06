//! Coordinator: drives the 5-stage pipeline per commit.

mod actor;

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
pub(super) const PROGRESS_INTERVAL: usize = 25;

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
    /// Number of commits that encountered a per-commit error and were skipped.
    /// Plan 28 Task C.3: errors are isolated per-commit; the run still succeeds.
    pub commits_failed: usize,
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

        // Plan 28: delegate actor topology to actor::run_actor_pipeline.
        let actor_args = actor::ActorArgs {
            storage: self.storage.clone(),
            embedder: self.embedder.clone(),
            embed_batch: self.embed_batch,
            embed_mode: self.embed_mode,
            cache_storage: self.cache_storage.clone(),
            ignore_filter: self.ignore_filter.clone(),
            n_workers: self.workers.max(1),
            progress: self.progress.clone(),
        };
        let ar = actor::run_actor_pipeline(
            actor_args,
            commits,
            repo.clone(),
            commit_source,
            symbol_source,
            extractor,
        )
        .await?;

        result.new_commits = ar.new_commits;
        result.commits_failed = ar.commits_failed;
        result.new_hunks = ar.new_hunks;
        result.total_diff_bytes = ar.total_diff_bytes;
        result.total_added_lines = ar.total_added_lines;
        result.timings.diff_extract_ms = ar.diff_extract_ms;
        result.timings.embed_ms = ar.embed_ms;
        result.timings.storage_write_ms = ar.storage_write_ms;

        tracing::info!(
            new_commits = result.new_commits,
            commits_failed = result.commits_failed,
            "indexer: {} commits indexed, {} commits skipped due to errors",
            result.new_commits,
            result.commits_failed,
        );

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
    /// `actor::run_commit_owned` instead.
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
            result.total_added_lines += actor::count_added_lines(&rec.diff_text);
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

#[cfg(test)]
mod tests;
