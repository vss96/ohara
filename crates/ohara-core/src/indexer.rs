use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;

/// Source of commits + hunks. Implemented by `ohara-git` in a later task; defined
/// here so `ohara-core` stays git-free.
#[async_trait::async_trait]
pub trait CommitSource: Send + Sync {
    /// Yield commits in parents-first order, optionally starting after `since`.
    async fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>>;
    /// Yield the per-file hunks of a single commit.
    async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>>;
}

/// Source of HEAD symbols. Implemented by `ohara-parse` driver in a later task.
#[async_trait::async_trait]
pub trait SymbolSource: Send + Sync {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>>;
}

/// Optional UI / observer hook for `Indexer::run` long-running passes.
/// The CLI uses this to render an `indicatif` progress bar; the MCP
/// server passes a no-op so its output stays purely structured tracing.
/// Calls are best-effort — the indexer doesn't depend on the sink for
/// correctness.
pub trait ProgressSink: Send + Sync {
    /// Called once at the start of the commit walk.
    fn start(&self, total_commits: usize);
    /// Called after each commit is fully persisted.
    fn commit_done(&self, commits_done: usize, total_hunks: usize);
    /// Called when the commit walk finishes and HEAD-symbol extraction begins.
    fn phase_symbols(&self);
    /// Called once at the end (success or after final flush).
    fn finish(&self, total_commits: usize, total_hunks: usize, head_symbols: usize);
}

/// No-op `ProgressSink` for callers that don't want UI (MCP server,
/// tests). Constructible without any deps.
pub struct NullProgress;

impl ProgressSink for NullProgress {
    fn start(&self, _: usize) {}
    fn commit_done(&self, _: usize, _: usize) {}
    fn phase_symbols(&self) {}
    fn finish(&self, _: usize, _: usize, _: usize) {}
}

pub struct Indexer {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn EmbeddingProvider>,
    batch_commits: usize,
    /// Reserved knob for capping per-batch embedder calls; not yet wired into
    /// the loop (the inner per-commit batch is bounded by hunk count today).
    #[allow(dead_code)]
    embed_batch: usize,
    progress: Arc<dyn ProgressSink>,
}

impl Indexer {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            storage,
            embedder,
            batch_commits: 512,
            embed_batch: 32,
            progress: Arc::new(NullProgress),
        }
    }

    /// Attach a progress sink (CLI bar, structured event emitter, etc.).
    pub fn with_progress(mut self, p: Arc<dyn ProgressSink>) -> Self {
        self.progress = p;
        self
    }

    /// Override the per-batch commit count. Smaller values reduce peak
    /// memory at the cost of more transactions; larger values are faster
    /// but use more RAM. Default 512.
    pub fn with_batch_commits(mut self, n: usize) -> Self {
        self.batch_commits = n.max(1);
        self
    }

    /// Run a (full or incremental) indexing pass for `repo_id`.
    /// `commit_source` and `symbol_source` are wired by the caller.
    pub async fn run(
        &self,
        repo_id: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<IndexerReport> {
        let status = self.storage.get_index_status(repo_id).await?;
        let commits = commit_source
            .list_commits(status.last_indexed_commit.as_deref())
            .await?;
        let total_commits = commits.len();
        tracing::info!(new_commits = total_commits, "begin index pass");
        self.progress.start(total_commits);

        let mut latest_sha: Option<String> = status.last_indexed_commit.clone();
        let mut total_hunks = 0usize;
        let mut commits_done = 0usize;
        // Liveness signal for long runs: emit an info-level event every
        // PROGRESS_INTERVAL commits so `RUST_LOG=info` users see steady
        // output. Without this the indexer is silent for minutes on
        // first-time runs against real-world repos (~5k+ commits).
        const PROGRESS_INTERVAL: usize = 100;

        for chunk in commits.chunks(self.batch_commits) {
            for cm in chunk {
                let hunks = commit_source.hunks_for_commit(&cm.sha).await?;
                total_hunks += hunks.len();

                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(hunks.iter().map(|h| h.diff_text.clone()))
                    .collect();
                let embs = self.embedder.embed_batch(&texts).await?;
                let (msg_emb, hunk_embs) = embs.split_first().expect("non-empty");

                self.storage
                    .put_commit(
                        repo_id,
                        &CommitRecord {
                            meta: cm.clone(),
                            message_emb: msg_emb.clone(),
                        },
                    )
                    .await?;

                let records: Vec<HunkRecord> = hunks
                    .into_iter()
                    .zip(hunk_embs.iter().cloned())
                    .map(|(h, e)| HunkRecord {
                        hunk: h,
                        diff_emb: e,
                    })
                    .collect();
                self.storage.put_hunks(repo_id, &records).await?;
                latest_sha = Some(cm.sha.clone());
                commits_done += 1;
                self.progress.commit_done(commits_done, total_hunks);
                if commits_done % PROGRESS_INTERVAL == 0 {
                    tracing::info!(
                        commits_done,
                        total_commits,
                        total_hunks,
                        "indexing progress"
                    );
                    // Resume safety: advance the watermark periodically
                    // so a Ctrl-C / kill / crash mid-walk doesn't force
                    // the next run to redo every already-indexed commit.
                    // Combined with put_hunks's resume-clear semantics,
                    // the worst case after abort is re-doing at most
                    // PROGRESS_INTERVAL commits.
                    if let Some(sha) = latest_sha.as_deref() {
                        self.storage.set_last_indexed_commit(repo_id, sha).await?;
                    }
                }
            }
        }

        tracing::info!(
            total_commits,
            total_hunks,
            "commit walk done; extracting HEAD symbols"
        );
        self.progress.phase_symbols();
        let symbols = symbol_source.extract_head_symbols().await?;
        self.storage.put_head_symbols(repo_id, &symbols).await?;

        if let Some(sha) = latest_sha.as_deref() {
            self.storage.set_last_indexed_commit(repo_id, sha).await?;
        }

        self.progress
            .finish(total_commits, total_hunks, symbols.len());
        Ok(IndexerReport {
            new_commits: commits.len(),
            new_hunks: total_hunks,
            head_symbols: symbols.len(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct IndexerReport {
    pub new_commits: usize,
    pub new_hunks: usize,
    pub head_symbols: usize,
}
