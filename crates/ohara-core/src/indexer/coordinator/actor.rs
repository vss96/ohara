//! Actor-pipeline internals for the coordinator.
//!
//! This module holds the worker-level types and the spawned task topology
//! that backs `Coordinator::run_timed_with_extractor`. It is `pub(super)`
//! — callers outside the coordinator module must use the public
//! `Coordinator` API.

use std::sync::Arc;
use std::time::Instant;

use crate::indexer::stages::{
    attribute::AttributeStage, embed::EmbedStage, hunk_chunk::HunkChunkStage, persist::PersistStage,
};
use crate::indexer::{AtomicSymbolExtractor, CommitSource, ProgressSink, SymbolSource};
use crate::types::{CommitMeta, RepoId};
use crate::{EmbeddingProvider, Result, Storage};

use super::PROGRESS_INTERVAL;

/// Per-commit result returned by `run_commit_owned`.
#[derive(Default)]
pub(super) struct CommitWorkResult {
    pub(super) new_hunks: usize,
    pub(super) total_diff_bytes: u64,
    pub(super) total_added_lines: u64,
    /// True when the commit was fully persisted (not 100%-ignored).
    pub(super) persisted: bool,
    /// Per-commit timing contributions.
    pub(super) diff_extract_ms: u64,
    pub(super) embed_ms: u64,
    pub(super) storage_write_ms: u64,
}

/// Aggregate per-worker results.
#[derive(Default)]
pub(super) struct WorkerResult {
    pub(super) local: CommitWorkResult,
    pub(super) succeeded: usize,
    /// Number of per-commit errors encountered by this worker.
    pub(super) failed: usize,
}

/// Arguments for `run_actor_pipeline` — bundles the `Coordinator`
/// fields that the actor topology needs without threading `&self`
/// into spawned tasks.
pub(super) struct ActorArgs {
    pub(super) storage: Arc<dyn Storage + Send + Sync>,
    pub(super) embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    pub(super) embed_batch: usize,
    pub(super) embed_mode: crate::EmbedMode,
    pub(super) cache_storage: Option<Arc<dyn crate::Storage>>,
    pub(super) ignore_filter: Option<Arc<dyn crate::IgnoreFilter>>,
    pub(super) n_workers: usize,
    pub(super) progress: Arc<dyn ProgressSink>,
}

/// Return type for `run_actor_pipeline` — partial counts that
/// `run_timed_with_extractor` folds into `CoordinatorResult`.
pub(super) struct ActorResult {
    pub(super) new_commits: usize,
    pub(super) commits_failed: usize,
    pub(super) new_hunks: usize,
    pub(super) total_diff_bytes: u64,
    pub(super) total_added_lines: u64,
    pub(super) diff_extract_ms: u64,
    pub(super) embed_ms: u64,
    pub(super) storage_write_ms: u64,
}

/// Spawn the walker task and `n_workers` worker tasks, then collect
/// results. All filtering (already-indexed skip, ignore-filter) runs
/// inside the pipeline; the caller has already recorded
/// `walk_last_sha` before calling here.
pub(super) async fn run_actor_pipeline(
    args: ActorArgs,
    commits: Vec<CommitMeta>,
    repo: RepoId,
    commit_source: Arc<dyn CommitSource>,
    symbol_source: Arc<dyn SymbolSource>,
    extractor: Arc<dyn AtomicSymbolExtractor>,
) -> Result<ActorResult> {
    let n_workers = args.n_workers;
    let (tx, rx) = tokio::sync::mpsc::channel::<CommitMeta>(n_workers);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    // Walker task: filter already-indexed commits and send to workers.
    let storage_for_walker = args.storage.clone();
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

    // Worker tasks: pull CommitMeta values and run the per-commit pipeline.
    let mut worker_handles = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let rx_for_worker = rx.clone();
        let storage = args.storage.clone();
        let embedder = args.embedder.clone();
        let embed_batch = args.embed_batch;
        let embed_mode = args.embed_mode;
        let cache_storage = args.cache_storage.clone();
        let ignore_filter = args.ignore_filter.clone();
        let commit_source_arc = commit_source.clone();
        let symbol_source_arc = symbol_source.clone();
        let extractor_arc = extractor.clone();
        let repo_owned = repo.clone();
        let progress_arc = args.progress.clone();
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
                        let done =
                            commits_done_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
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
                        wr.failed += 1;
                    }
                }
            }
            wr
        }));
    }

    let _ = walker_handle.await;

    let mut out = ActorResult {
        new_commits: 0,
        commits_failed: 0,
        new_hunks: 0,
        total_diff_bytes: 0,
        total_added_lines: 0,
        diff_extract_ms: 0,
        embed_ms: 0,
        storage_write_ms: 0,
    };
    for handle in worker_handles {
        if let Ok(wr) = handle.await {
            out.new_commits += wr.succeeded;
            out.commits_failed += wr.failed;
            out.new_hunks += wr.local.new_hunks;
            out.total_diff_bytes += wr.local.total_diff_bytes;
            out.total_added_lines += wr.local.total_added_lines;
            out.diff_extract_ms += wr.local.diff_extract_ms;
            out.embed_ms += wr.local.embed_ms;
            out.storage_write_ms += wr.local.storage_write_ms;
        }
    }
    Ok(out)
}

/// Free async function that runs stages 2-5 for a single commit.
/// Used by the actor worker tasks spawned in `run_actor_pipeline`.
/// The walker has already done the `commit_exists` check, so it is not
/// repeated here.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_commit_owned(
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
        total_added_lines += count_added_lines(&rec.diff_text);
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
pub(super) fn count_added_lines(diff_text: &str) -> u64 {
    diff_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count() as u64
}
