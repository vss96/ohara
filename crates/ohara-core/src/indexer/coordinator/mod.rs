//! Coordinator: drives the 5-stage pipeline per commit.

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

    /// Run the full 5-stage pipeline for all commits in `source` that
    /// follow the resume watermark.
    ///
    /// Returns `()` — use `run_timed` when `IndexerReport` fields are needed.
    pub async fn run(
        &self,
        repo: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<()> {
        self.run_timed_with_extractor(
            repo,
            commit_source,
            symbol_source,
            &NullAtomicSymbolExtractor,
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
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<CoordinatorResult> {
        self.run_timed_with_extractor(
            repo,
            commit_source,
            symbol_source,
            &NullAtomicSymbolExtractor,
        )
        .await
    }

    /// Like `run_timed` but accepts a custom `AtomicSymbolExtractor`
    /// for ExactSpan attribution. Used by `Indexer::run` to pass the
    /// configured `symbol_extractor` through to the attribute stage.
    pub async fn run_timed_with_extractor(
        &self,
        repo: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        extractor: &dyn AtomicSymbolExtractor,
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
        let commits = CommitWalkStage::run(commit_source, watermark.as_ref()).await?;
        result.timings.commit_walk_ms = walk_start.elapsed().as_millis() as u64;
        tracing::info!(
            new_commits = commits.len(),
            walk_ms = result.timings.commit_walk_ms,
            "commit walk complete; begin per-commit indexing"
        );
        self.progress.start(commits.len());

        let mut commits_done = 0usize;
        for commit in &commits {
            // Skip commits that are already indexed.
            if self.storage.commit_exists(&commit.commit_sha).await? {
                tracing::debug!(sha = %commit.commit_sha, "plan-19: skipping already-indexed commit");
                result.latest_sha = Some(commit.commit_sha.clone());
                commits_done += 1;
                self.progress.commit_done(commits_done, result.new_hunks);
                if commits_done % PROGRESS_INTERVAL == 0 {
                    tracing::info!(
                        commits_done,
                        total = commits.len(),
                        new_hunks = result.new_hunks,
                        "indexing progress"
                    );
                }
                continue;
            }
            self.run_commit_timed(
                repo,
                commit,
                commit_source,
                symbol_source,
                extractor,
                &mut result,
            )
            .await?;
            result.latest_sha = Some(commit.commit_sha.clone());
            commits_done += 1;
            self.progress.commit_done(commits_done, result.new_hunks);
            if commits_done % PROGRESS_INTERVAL == 0 {
                tracing::info!(
                    commits_done,
                    total = commits.len(),
                    new_hunks = result.new_hunks,
                    "indexing progress"
                );
            }
        }
        Ok(result)
    }

    /// Run stages 2-5 for a single commit, accumulating timing and counts.
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
        // Mixed commits keep their non-ignored hunks; pure-ignored
        // commits are caught by the `paths_kept == 0` branch below.
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

        // Stage 4: embed (timed). Create the embed stage here rather than
        // accepting it as a parameter to keep argument count within limits.
        let embed_stage = EmbedStage::new(self.embedder.clone()).with_embed_batch(self.embed_batch);
        let embed_start = Instant::now();
        let embed_output = embed_stage.run(&commit.message, &attributed).await?;
        result.timings.embed_ms += embed_start.elapsed().as_millis() as u64;

        // Stage 5: persist (timed).
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

/// Count "+"-prefixed lines in a unified-diff snippet (excludes `+++` headers).
fn count_added_lines_stage(diff_text: &str) -> u64 {
    diff_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count() as u64
}

#[cfg(test)]
mod tests;
