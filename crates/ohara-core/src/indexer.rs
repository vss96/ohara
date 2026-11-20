pub mod coordinator;
pub mod stages;
mod tip_context;

pub use tip_context::TipContext;

use crate::index_metadata::RuntimeIndexMetadata;
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;
use std::time::Instant;

/// Plan 15 Task C.1: maximum post-image source size (in bytes) the
/// indexer will hand to `AtomicSymbolExtractor::extract`. Sources
/// larger than this fall through to header-only attribution
/// (`AttributionInputs { symbols: None, source: None }`), which is
/// the same path used when `file_at_commit` returns `Ok(None)`.
///
/// 2 MiB is large enough to cover every hand-written source file in
/// the languages we support and small enough to keep tree-sitter's
/// AST allocation bounded for vendor drops, generated bundles, and
/// minified blobs. No CLI override today — operators who hit this
/// cap can edit the constant and rebuild. A flag will be added if
/// real workloads show a need.
pub const MAX_ATTRIBUTABLE_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Source of commits + hunks. Implemented by `ohara-git` in a later task; defined
/// here so `ohara-core` stays git-free.
#[async_trait::async_trait]
pub trait CommitSource: Send + Sync {
    /// Yield commits in parents-first order, optionally starting after `since`.
    async fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>>;
    /// Commits reachable from HEAD but from none of the `hide` tips, oldest
    /// first. Default falls back to the first hide entry as a watermark so
    /// single-tip fakes keep working.
    async fn list_commits_hiding(&self, hide: &[String]) -> Result<Vec<CommitMeta>> {
        self.list_commits(hide.first().map(String::as_str)).await
    }
    /// Like [`Self::list_commits_hiding`] but walks from commit `from`
    /// instead of the live HEAD. Default ignores `from` so fakes keep working.
    async fn list_commits_from_hiding(
        &self,
        _from: &str,
        hide: &[String],
    ) -> Result<Vec<CommitMeta>> {
        self.list_commits_hiding(hide).await
    }
    /// Yield the per-file hunks of a single commit.
    async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>>;

    /// Plan 11: post-image file content at `sha` for `path`. Returns
    /// `Ok(None)` when the file doesn't exist at that commit (deleted,
    /// renamed-away) so callers can fall back to header-only
    /// attribution gracefully. Default implementation returns
    /// `Ok(None)` so legacy fakes that don't care about per-hunk
    /// symbol attribution don't have to wire it up.
    async fn file_at_commit(&self, _sha: &str, _path: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Source of HEAD symbols. Implemented by `ohara-parse` driver in a later task.
#[async_trait::async_trait]
pub trait SymbolSource: Send + Sync {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>>;

    /// Per-file HEAD symbol lookup for the attribute stage.
    ///
    /// Default implementation returns an empty Vec — callers fall back
    /// to no symbol attribution. Implementations backed by a pre-built
    /// symbol index can override this for fast per-file lookup without
    /// re-parsing the whole tree.
    async fn head_symbols_for_path(&self, _path: &str) -> Result<Vec<Symbol>> {
        Ok(vec![])
    }
}

/// Plan 11: per-file atomic-symbol extractor used during per-hunk
/// symbol attribution. Implemented by `ohara-parse` (which calls
/// `extract_atomic_symbols` for the language matching `file_path`'s
/// extension). Synchronous — tree-sitter parsing is CPU-bound and
/// the indexer's outer per-hunk loop is async.
pub trait AtomicSymbolExtractor: Send + Sync {
    /// Extract atomic (pre-merge) symbols for `path`'s post-image
    /// `source`. An empty Vec means "no symbols recoverable" — the
    /// attributor falls back to header-only attribution. Implementations
    /// MUST NOT panic on parse errors; they should return an empty
    /// Vec and let the attributor's HunkHeader path take over.
    fn extract(&self, path: &str, source: &str) -> Vec<Symbol>;
}

/// No-op extractor for callers that don't want per-hunk attribution
/// (test fakes, indexers that prefer the cheaper header-only path).
pub struct NullAtomicSymbolExtractor;

impl AtomicSymbolExtractor for NullAtomicSymbolExtractor {
    fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
        Vec::new()
    }
}

/// Optional UI / observer hook for `Indexer::run` long-running passes.
/// The CLI uses this to render an `indicatif` progress bar; the MCP
/// server passes a no-op so its output stays purely structured tracing.
/// Calls are best-effort — the indexer doesn't depend on the sink for
/// correctness.
pub trait ProgressSink: Send + Sync {
    /// Pre-walk phase indicator: shown as a length-less spinner during
    /// long startup phases (model load, commit walk) before the
    /// per-commit bar materialises. Default impl is a no-op so existing
    /// callers stay source-compatible. The `msg` is what the user sees
    /// next to the spinner — keep it short.
    fn pre_walk(&self, _msg: &str) {}
    /// Called once when the per-commit indexing pass begins (after the
    /// commit walk has produced a known total).
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
    embed_batch: usize,
    progress: Arc<dyn ProgressSink>,
    /// Plan 13: optional runtime metadata recorded at the end of a
    /// successful index pass. Left unset by tests that don't care about
    /// compatibility tracking; the CLI / MCP wire it from
    /// `index_metadata::runtime_metadata_from(...)`.
    runtime_metadata: Option<RuntimeIndexMetadata>,
    /// Plan 11: per-file atomic-symbol extractor used to derive
    /// ExactSpan hunk-symbol attribution. Defaults to the
    /// header-only `NullAtomicSymbolExtractor` so legacy callers and
    /// test fakes don't have to wire tree-sitter just to run the
    /// indexer.
    symbol_extractor: Arc<dyn AtomicSymbolExtractor>,
    /// Repo root used to load `.oharaignore` / `.gitattributes`. When
    /// `None`, the coordinator runs without an ignore filter (today's
    /// behaviour). Plan 26.
    repo_root: Option<std::path::PathBuf>,
    /// Plan 27: chunk-embed cache mode. Threaded to Coordinator and
    /// from there to EmbedStage. Defaults to Off.
    embed_mode: crate::EmbedMode,
    /// Plan 28: number of worker tasks for the actor-style commit
    /// pipeline. Defaults to num_cpus::get() at builder time.
    workers: usize,
    /// Plan 33: per-worktree tip bookkeeping. `None` keeps the legacy
    /// single-watermark behaviour.
    tip_context: Option<TipContext>,
}

impl Indexer {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            storage,
            embedder,
            batch_commits: 512,
            embed_batch: 32,
            progress: Arc::new(NullProgress),
            runtime_metadata: None,
            symbol_extractor: Arc::new(NullAtomicSymbolExtractor),
            repo_root: None,
            embed_mode: crate::EmbedMode::default(),
            workers: num_cpus::get().max(1),
            tip_context: None,
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

    /// Plan 15 Task B.1: cap the per-commit embedder call size.
    /// `Indexer::run` slices each commit's text inputs (commit
    /// message + every hunk's `semantic_text`) into chunks of at
    /// most `n`, calls `embed_batch` once per chunk, and concatenates
    /// the results. `n=0` is normalised to `1` (degenerate but
    /// safe). Default 32; lower values cap peak embedder allocation
    /// at the cost of more `embed_batch` calls per commit.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Plan 13: record the supplied runtime metadata to
    /// `Storage::put_index_metadata` at the end of a successful index
    /// pass. Set by callers that want the post-run compatibility check
    /// to find current-binary metadata in the index.
    pub fn with_runtime_metadata(mut self, meta: RuntimeIndexMetadata) -> Self {
        self.runtime_metadata = Some(meta);
        self
    }

    /// Plan 11: attach a per-file atomic-symbol extractor so the
    /// indexer can produce `ExactSpan` hunk-symbol attribution. When
    /// unset, the indexer falls back to header-only attribution
    /// (which still produces useful `HunkHeader`-confidence rows
    /// when git's diff format includes the enclosing-function context).
    pub fn with_atomic_symbol_extractor(
        mut self,
        extractor: Arc<dyn AtomicSymbolExtractor>,
    ) -> Self {
        self.symbol_extractor = extractor;
        self
    }

    /// Set the repo root from which `LayeredIgnore::load` reads
    /// `.gitattributes` and `.oharaignore`. Plan 26.
    pub fn with_repo_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }

    /// Set the chunk-embed cache mode. Plan 27.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        self
    }

    /// Set the number of worker tasks. `n.max(1)` is enforced.
    /// Plan 28.
    pub fn with_workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }

    /// Plan 33: index this run as one worktree of a shared store. The walk
    /// hides every recorded tip and the worktree's HEAD is recorded as its
    /// tip afterwards (instead of the legacy watermark).
    pub fn with_tip_context(mut self, context: TipContext) -> Self {
        self.tip_context = Some(context);
        self
    }

    /// Run a (full or incremental) indexing pass for `repo_id`.
    /// `commit_source` and `symbol_source` are wired by the caller.
    ///
    /// Delegates the per-commit pipeline (stages 1-5) to `Coordinator`,
    /// keeping HEAD-symbol extraction, progress sink, runtime metadata,
    /// and `IndexerReport` hydration in this wrapper (plan-19 C.2).
    pub async fn run(
        &self,
        repo_id: &RepoId,
        commit_source: Arc<dyn CommitSource>,
        symbol_source: Arc<dyn SymbolSource>,
    ) -> Result<IndexerReport> {
        use crate::indexer::coordinator::Coordinator;

        // Delegate stages 1-5 to the coordinator. Pass the progress
        // sink through so the coordinator can drive `pre_walk`,
        // `start`, and `commit_done` from inside the per-commit loop.
        // Issue #29 — without this thread-through the bar only got
        // `start` / `commit_done(total, total)` / `finish` called at
        // the end of the run, so the user saw no per-commit motion.
        let mut coordinator = Coordinator::new(self.storage.clone(), self.embedder.clone())
            .with_embed_batch(self.embed_batch)
            .with_progress(self.progress.clone())
            .with_embed_mode(self.embed_mode)
            .with_workers(self.workers);
        if let Some(ctx) = self.tip_context.as_ref() {
            coordinator = coordinator
                .with_hidden_tips(ctx.hidden(repo_id).await?)
                .with_walk_from(ctx.head.clone());
        }
        // Plan 26: when repo_root is set, build a LayeredIgnore filter
        // and thread it to the Coordinator. Best-effort load — a missing
        // `.oharaignore` is fine (LayeredIgnore::load treats ENOENT as
        // empty).
        if let Some(root) = self.repo_root.as_ref() {
            let filter = crate::ignore::LayeredIgnore::load(root)?;
            coordinator = coordinator.with_ignore_filter(std::sync::Arc::new(filter));
        }

        let result = coordinator
            .run_timed_with_extractor(
                repo_id,
                commit_source.clone(),
                symbol_source.clone(),
                self.symbol_extractor.clone(),
            )
            .await?;

        let mut timings = result.timings;
        timings.total_diff_bytes = result.total_diff_bytes;
        timings.total_added_lines = result.total_added_lines;

        let total_commits = result.new_commits;
        let total_hunks = result.new_hunks;

        tracing::info!(
            total_commits,
            total_hunks,
            "commit walk done; extracting HEAD symbols"
        );
        self.progress.phase_symbols();

        // HEAD symbol extraction (outside coordinator scope). See
        // `TipContext::owns_head_symbols` for which worktree owns the
        // snapshot. Extract first, then replace the snapshot in one
        // storage transaction: a failed extraction leaves the previous
        // snapshot intact, and queries never see it empty.
        let symbols_start = Instant::now();
        let owns_head_symbols = match self.tip_context.as_ref() {
            Some(ctx) => ctx.owns_head_symbols(repo_id).await?,
            None => true,
        };
        let symbols = match owns_head_symbols {
            true => symbol_source.extract_head_symbols().await?,
            false => Vec::new(),
        };
        if owns_head_symbols {
            self.storage.put_head_symbols(repo_id, &symbols).await?;
        }
        timings.head_symbols_ms = symbols_start.elapsed().as_millis() as u64;

        // Advance the watermark (legacy) or record this worktree's tip.
        match self.tip_context.as_ref() {
            Some(ctx) => ctx.record(repo_id, result.commits_failed).await?,
            None => {
                if let Some(sha) = result.latest_sha.as_deref() {
                    self.storage.set_last_indexed_commit(repo_id, sha).await?;
                }
            }
        }

        // Plan 13: write runtime metadata only on full success.
        if let Some(meta) = &self.runtime_metadata {
            self.storage
                .put_index_metadata(repo_id, &meta.to_storage_components())
                .await?;
        }

        self.progress
            .finish(total_commits, total_hunks, symbols.len());
        Ok(IndexerReport {
            new_commits: total_commits,
            new_hunks: total_hunks,
            head_symbols: symbols.len(),
            commits_failed: result.commits_failed,
            phase_timings: timings,
        })
    }
}

/// Plan 15: slice `texts` into chunks of `cap` and embed each in
/// turn, concatenating the resulting vectors so the caller sees the
/// same `Vec<Vec<f32>>` it would have received from a single
/// `embed_batch(&texts)` call. Bounds peak per-commit embedder
/// allocation: a 5,000-hunk vendor drop with `cap=32` issues 157
/// embedder calls of <= 32 strings rather than one call of 5,001.
///
/// `cap == 0` is treated as `cap == 1` (degenerate but safe — every
/// text is its own chunk).
///
/// Retained for tests in phase_timing_tests; production code now uses
/// `EmbedStage::embed_in_chunks` (plan-19 B.4).
#[cfg(test)]
async fn embed_in_chunks(
    embedder: &dyn EmbeddingProvider,
    texts: &[String],
    cap: usize,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let cap = cap.max(1);
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(cap) {
        // We allocate `chunk_owned` here rather than passing
        // `chunk` directly because `EmbeddingProvider::embed_batch`
        // takes `&[String]`; it already clones internally for
        // `spawn_blocking`, so this allocation is unavoidable
        // without changing the trait. Keeping it inside the loop
        // means each iteration's `Vec<String>` clone is freed
        // before the next chunk is fetched, bounding the
        // *embedder's* working set per call to `cap` strings.
        // (The outer `texts: Vec<String>` in `Indexer::run` is
        // still O(commit-size); a future task can move
        // semantic-text construction inside this loop to also
        // bound that.)
        let chunk_owned: Vec<String> = chunk.to_vec();
        let mut embs = embedder.embed_batch(&chunk_owned).await?;
        if embs.len() != chunk_owned.len() {
            return Err(crate::OhraError::Embedding(format!(
                "embed_batch returned {} vectors for {} inputs",
                embs.len(),
                chunk_owned.len()
            )));
        }
        out.append(&mut embs);
    }
    Ok(out)
}

/// Count "+"-prefixed lines in a unified-diff snippet. Excludes the
/// `+++ b/path` file header (a `+++` line is not a content add) so the
/// ratio reported as `total_diff_bytes / total_added_lines` reflects
/// real changed lines, not the metadata that git2 always emits.
///
/// Retained for tests; production code uses the coordinator's inline
/// equivalent (plan-19 C.2).
#[cfg(test)]
fn count_added_lines(diff_text: &str) -> u64 {
    diff_text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count() as u64
}

#[derive(Debug, Clone)]
pub struct IndexerReport {
    pub new_commits: usize,
    pub new_hunks: usize,
    pub head_symbols: usize,
    /// Plan 31: commits the parallel workers could not persist (e.g. a
    /// storage write that exhausted `busy_timeout`). Non-zero means the
    /// index is incomplete; the CLI surfaces this and exits non-zero so
    /// the loss is never silent.
    pub commits_failed: usize,
    /// Per-phase wall-time + input-size breakdown for the run, captured
    /// when the `--profile` flag (or any caller plumbing
    /// `Indexer::run`) wants the same numbers used for throughput
    /// analysis. Always present so consumers don't branch on
    /// `Option`; fields default to zero on a no-op pass.
    pub phase_timings: PhaseTimings,
}

/// Per-phase wall-time + input-size breakdown for one `Indexer::run`
/// invocation. Field semantics are cumulative across the commit walk:
/// `embed_ms` is the sum of every per-commit `embed_batch` call, etc.
/// `total_diff_bytes / total_added_lines` gives the hunk-text inflation
/// ratio (high ratio ⇒ the embedder is seeing more context bytes than
/// signal-bearing added lines, a hint to trim git2 context).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PhaseTimings {
    /// Wall-time spent listing commits (`CommitSource::list_commits`).
    pub commit_walk_ms: u64,
    /// Wall-time spent extracting per-commit hunks
    /// (`CommitSource::hunks_for_commit`).
    pub diff_extract_ms: u64,
    /// Wall-time attributed to tree-sitter parsing during hunk
    /// extraction. Reserved: today the parser only runs during HEAD
    /// symbol extraction (`head_symbols_ms`); this field exists so a
    /// future per-hunk parse step has somewhere to land without
    /// changing the report shape.
    pub tree_sitter_parse_ms: u64,
    /// Wall-time spent inside `EmbeddingProvider::embed_batch` calls.
    pub embed_ms: u64,
    /// Wall-time spent writing commit + hunk rows
    /// (`StorageWrite::put_commit_with_hunks`, one transaction per commit
    /// in the SQLite backend).
    pub storage_write_ms: u64,
    /// Wall-time spent inside FTS5 inserts. Reserved: today the FTS
    /// insert is bundled inside `put_hunks` and rolls into
    /// `storage_write_ms`. Kept here so a future split (e.g. deferred
    /// FTS index build) has a slot.
    pub fts_insert_ms: u64,
    /// Wall-time spent extracting + persisting HEAD symbols
    /// (`SymbolSource::extract_head_symbols` + `put_head_symbols`).
    pub head_symbols_ms: u64,
    /// Sum of every diff-text byte-length fed to the embedder during
    /// the run. Pairs with `total_added_lines` to compute
    /// `bytes_per_added_line`, the inflation-ratio diagnostic.
    pub total_diff_bytes: u64,
    /// Sum of every "+"-prefixed line count across all hunks during
    /// the run. Counts only added lines (context / removed lines are
    /// excluded) so the resulting ratio reflects "bytes the embedder
    /// sees per signal-bearing line".
    pub total_added_lines: u64,
}

#[cfg(test)]
mod count_added_lines_tests {
    #[test]
    fn counts_plus_prefixed_lines_only() {
        let diff =
            "--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1,2 @@\n+added one\n+added two\n context\n-removed\n";
        assert_eq!(super::count_added_lines(diff), 2);
    }

    #[test]
    fn empty_diff_is_zero() {
        assert_eq!(super::count_added_lines(""), 0);
    }
}

#[cfg(test)]
mod phase_timing_tests {
    use super::*;
    use crate::query::IndexStatus;
    use crate::storage::{CommitRecord, HunkRecord, StorageWrite};
    use crate::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol};
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::Mutex;

    struct FakeCommitSource {
        commits: Vec<CommitMeta>,
        hunks: Vec<Hunk>,
        /// Sleep applied inside `list_commits` / `hunks_for_commit` so
        /// the wall-time clocks observe a non-zero value even on a
        /// fast box. Without this, the test races against the tick
        /// resolution on Linux CI.
        sleep_per_call: std::time::Duration,
    }

    #[async_trait]
    impl CommitSource for FakeCommitSource {
        async fn list_commits(&self, _since: Option<&str>) -> Result<Vec<CommitMeta>> {
            std::thread::sleep(self.sleep_per_call);
            Ok(self.commits.clone())
        }
        async fn hunks_for_commit(&self, _sha: &str) -> Result<Vec<Hunk>> {
            std::thread::sleep(self.sleep_per_call);
            Ok(self.hunks.clone())
        }
    }

    struct FakeSymbolSource {
        symbols: Vec<Symbol>,
        sleep: std::time::Duration,
    }

    #[async_trait]
    impl SymbolSource for FakeSymbolSource {
        async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
            std::thread::sleep(self.sleep);
            Ok(self.symbols.clone())
        }
    }

    struct FakeEmbedder {
        sleep: std::time::Duration,
    }

    #[async_trait]
    impl crate::EmbeddingProvider for FakeEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "fake"
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            std::thread::sleep(self.sleep);
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }

    struct FakeStorage {
        write_sleep: std::time::Duration,
        last_indexed: Mutex<Option<String>>,
        /// Plan 9: SHAs that already have a `commit_record` row, so the
        /// indexer's resume short-circuit can ask "did we index this
        /// already?" via `commit_exists`. `put_commit` adds to the set on
        /// the write path so end-to-end tests don't have to pre-seed.
        seen_commits: Mutex<HashSet<String>>,
        /// Plan 13: most-recent argument the indexer passed to
        /// `put_index_metadata`. `None` means "no metadata write
        /// happened" — the partial-failure test checks this stays
        /// unchanged across a failed run.
        last_metadata: Mutex<Option<Vec<(String, String)>>>,
        /// Number of `put_head_symbols` calls (plan-33 tip tests).
        head_puts: Mutex<usize>,
        /// The stored HEAD-symbol snapshot.
        head_snapshot: Mutex<Vec<Symbol>>,
    }

    impl FakeStorage {
        fn new(write_sleep: std::time::Duration) -> Self {
            Self {
                write_sleep,
                last_indexed: Mutex::new(None),
                seen_commits: Mutex::new(HashSet::new()),
                last_metadata: Mutex::new(None),
                head_puts: Mutex::new(0),
                head_snapshot: Mutex::new(Vec::new()),
            }
        }

        fn with_seen<I: IntoIterator<Item = String>>(self, shas: I) -> Self {
            self.seen_commits.lock().unwrap().extend(shas);
            self
        }
    }

    #[async_trait]
    impl crate::storage::StorageStatus for FakeStorage {
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
        }

        async fn get_index_metadata(
            &self,
            _: &RepoId,
        ) -> Result<crate::index_metadata::StoredIndexMetadata> {
            Ok(crate::index_metadata::StoredIndexMetadata::default())
        }

        async fn put_index_metadata(
            &self,
            _: &RepoId,
            components: &[(String, String)],
        ) -> Result<()> {
            *self.last_metadata.lock().unwrap() = Some(components.to_vec());
            Ok(())
        }
    }

    #[async_trait]
    impl crate::storage::StorageWrite for FakeStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
            Ok(())
        }

        async fn set_last_indexed_commit(&self, _: &RepoId, sha: &str) -> Result<()> {
            *self.last_indexed.lock().unwrap() = Some(sha.to_string());
            Ok(())
        }

        async fn put_commit(&self, _: &RepoId, record: &CommitRecord) -> Result<()> {
            std::thread::sleep(self.write_sleep);
            self.seen_commits
                .lock()
                .unwrap()
                .insert(record.meta.commit_sha.clone());
            Ok(())
        }

        async fn commit_exists(&self, sha: &str) -> Result<bool> {
            Ok(self.seen_commits.lock().unwrap().contains(sha))
        }

        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> Result<()> {
            std::thread::sleep(self.write_sleep);
            Ok(())
        }

        async fn put_head_symbols(&self, _: &RepoId, symbols: &[Symbol]) -> Result<()> {
            *self.head_puts.lock().unwrap() += 1;
            *self.head_snapshot.lock().unwrap() = symbols.to_vec();
            std::thread::sleep(self.write_sleep);
            Ok(())
        }
    }

    #[async_trait]
    impl crate::storage::StorageRead for FakeStorage {
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<crate::HunkHit>> {
            Ok(vec![])
        }

        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<crate::HunkHit>> {
            Ok(vec![])
        }

        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<crate::HunkHit>> {
            Ok(vec![])
        }

        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<crate::HunkHit>> {
            Ok(vec![])
        }

        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<crate::HunkHit>> {
            Ok(vec![])
        }

        async fn get_hunk_symbols(
            &self,
            _: &RepoId,
            _: crate::storage::HunkId,
        ) -> Result<Vec<crate::types::HunkSymbol>> {
            Ok(vec![])
        }

        async fn get_hunk_symbols_batch(
            &self,
            _: &RepoId,
            _: &[crate::storage::HunkId],
        ) -> Result<std::collections::HashMap<crate::storage::HunkId, Vec<crate::types::HunkSymbol>>>
        {
            Ok(std::collections::HashMap::new())
        }
    }

    #[async_trait]
    impl crate::storage::StorageExplain for FakeStorage {
        async fn get_commit(&self, _: &RepoId, _: &str) -> Result<Option<CommitMeta>> {
            Ok(None)
        }

        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
        ) -> Result<Vec<Hunk>> {
            Ok(vec![])
        }

        async fn get_neighboring_file_commits(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
            _: u8,
            _: u8,
        ) -> Result<Vec<(u32, CommitMeta)>> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl crate::storage::StorageCache for FakeStorage {
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }

        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn fake_commit(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: Some("a@a".into()),
            ts: 1_700_000_000,
            message: format!("commit {sha}"),
        }
    }

    fn fake_hunk(sha: &str, diff_text: &str) -> Hunk {
        Hunk {
            commit_sha: sha.into(),
            file_path: "a.rs".into(),
            language: Some("rust".into()),
            change_kind: ChangeKind::Added,
            diff_text: diff_text.into(),
        }
    }

    #[tokio::test]
    async fn run_populates_phase_timings() {
        // Sleep ≥1ms inside every observed phase so the per-phase
        // wall-time clocks tick on every platform. Test asserts each
        // PhaseTimings field is non-zero — the contract from v0.6
        // Plan 6 Task 0.1.
        let sleep = std::time::Duration::from_millis(2);
        let storage = std::sync::Arc::new(FakeStorage::new(sleep));
        let embedder = std::sync::Arc::new(FakeEmbedder { sleep });

        let commit_source = FakeCommitSource {
            commits: vec![fake_commit("aaaa"), fake_commit("bbbb")],
            // Two added lines per commit so total_added_lines > 0.
            hunks: vec![fake_hunk("xxx", "+added line 1\n+added line 2\n")],
            sleep_per_call: sleep,
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep,
        };

        let indexer = Indexer::new(storage, embedder);
        let repo_id = RepoId::from_parts("first", "/tmp/fake-repo");
        let report = indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("indexer run");

        let pt = &report.phase_timings;
        assert!(pt.commit_walk_ms > 0, "commit_walk_ms must be populated");
        assert!(pt.diff_extract_ms > 0, "diff_extract_ms must be populated");
        assert!(pt.embed_ms > 0, "embed_ms must be populated");
        assert!(
            pt.storage_write_ms > 0,
            "storage_write_ms must be populated"
        );
        assert!(pt.head_symbols_ms > 0, "head_symbols_ms must be populated");
        // Hunk-text inflation inputs (Task 0.3): both must accumulate.
        assert!(
            pt.total_diff_bytes > 0,
            "total_diff_bytes must accumulate from observed hunks"
        );
        assert!(
            pt.total_added_lines > 0,
            "total_added_lines must accumulate '+'-prefixed lines"
        );
        // 2 commits × 2 added lines per hunk × 1 hunk per commit = 4.
        assert_eq!(pt.total_added_lines, 4);
    }

    struct EmptyEmbedder;

    #[async_trait]
    impl crate::EmbeddingProvider for EmptyEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "empty"
        }
        async fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            // Misbehaving embedder: returns empty for non-empty input.
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn run_returns_typed_error_when_embedder_drops_inputs() {
        // Plan 28 Task C.3: per-commit failure isolation means a buggy
        // embedder no longer causes `Indexer::run` to return an error.
        // Instead the commit is skipped (warn-logged) and the run
        // completes with new_commits == 0.
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::from_millis(0)));
        let embedder = std::sync::Arc::new(EmptyEmbedder);

        let commit_source = FakeCommitSource {
            commits: vec![fake_commit("aaaa")],
            hunks: vec![fake_hunk("xxx", "+x\n")],
            sleep_per_call: std::time::Duration::from_millis(0),
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::from_millis(0),
        };

        let indexer = Indexer::new(storage, embedder);
        let repo_id = RepoId::from_parts("first", "/tmp/empty-emb");
        let report = indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("run must succeed even when one commit has a broken embedder");
        assert_eq!(
            report.new_commits, 0,
            "broken embedder causes the commit to be skipped, not an error"
        );
    }

    /// Counts `embed_batch` calls so the skip-already-indexed regression
    /// can assert the embedder was hit exactly once per *new* commit.
    /// Returns the same shape FakeEmbedder does (one zero vector per
    /// input text) so the indexer's downstream split_first / persist
    /// paths still work.
    struct CountingEmbedder {
        calls: Mutex<usize>,
    }

    impl CountingEmbedder {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }
        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl crate::EmbeddingProvider for CountingEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "counting"
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            *self.calls.lock().unwrap() += 1;
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }

    #[tokio::test]
    async fn run_skips_already_indexed_commits_and_only_embeds_new_ones() {
        // Plan 9 Task 2.1: resume safety. The CommitSource returns three
        // SHAs but two of them already have `commit_record` rows from a
        // prior run (the merge-from-feature-branch case described in the
        // RFC). The indexer must short-circuit on those two and only fire
        // the embedder for the genuinely new SHA.
        let storage = std::sync::Arc::new(
            FakeStorage::new(std::time::Duration::from_millis(0))
                .with_seen(["already-a".to_string(), "already-b".to_string()]),
        );
        let embedder = std::sync::Arc::new(CountingEmbedder::new());

        let commit_source = FakeCommitSource {
            commits: vec![
                fake_commit("already-a"),
                fake_commit("already-b"),
                fake_commit("brand-new-c"),
            ],
            hunks: vec![fake_hunk("any", "+x\n")],
            sleep_per_call: std::time::Duration::from_millis(0),
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::from_millis(0),
        };

        let indexer = Indexer::new(storage.clone(), embedder.clone());
        let repo_id = RepoId::from_parts("first", "/tmp/skip-test");
        indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("indexer run");

        assert_eq!(
            embedder.call_count(),
            1,
            "embedder must run only for the one new commit, skipping the two pre-seeded SHAs"
        );
    }

    #[tokio::test]
    async fn watermark_advances_through_consecutive_skipped_commits() {
        // Plan 9 Task 2.1 Step 3: a long stretch of already-indexed
        // commits (no new work) must still leave a fresh watermark on
        // disk. Otherwise a Ctrl-C immediately after a skip stretch would
        // make the next resume re-walk them — correct but wasted work.
        // Drive enough commits to fire the periodic-flush check at least
        // once (PROGRESS_INTERVAL == 100), then assert the watermark
        // points at the last-walked sha.
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::from_millis(0)));
        // Pre-seed 150 commits — well past the 100-tick periodic flush.
        let shas: Vec<String> = (0..150).map(|i| format!("seen-{i:03}")).collect();
        for sha in &shas {
            storage.seen_commits.lock().unwrap().insert(sha.clone());
        }

        let commit_source = FakeCommitSource {
            commits: shas.iter().map(|s| fake_commit(s)).collect(),
            hunks: vec![],
            sleep_per_call: std::time::Duration::from_millis(0),
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::from_millis(0),
        };
        let embedder = std::sync::Arc::new(CountingEmbedder::new());

        let indexer = Indexer::new(storage.clone(), embedder.clone());
        let repo_id = RepoId::from_parts("first", "/tmp/skip-watermark");
        indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("indexer run");

        assert_eq!(
            embedder.call_count(),
            0,
            "no new commits — embedder must not be hit at all"
        );
        let last = storage.last_indexed.lock().unwrap().clone();
        assert_eq!(
            last.as_deref(),
            Some(shas.last().unwrap().as_str()),
            "watermark must advance to the final walked sha even through an all-skip run"
        );
    }

    fn fake_runtime_metadata() -> crate::index_metadata::RuntimeIndexMetadata {
        let mut parsers = std::collections::BTreeMap::new();
        parsers.insert("rust".to_string(), "1".to_string());
        crate::index_metadata::RuntimeIndexMetadata {
            schema_version: "3".into(),
            embedding_model: "fake-model".into(),
            embedding_dimension: 4,
            reranker_model: "fake-reranker".into(),
            chunker_version: "1".into(),
            semantic_text_version: "0".into(),
            parser_versions: parsers,
            embed_input_mode: "semantic".into(),
        }
    }

    #[tokio::test]
    async fn run_writes_runtime_metadata_on_success() {
        // Plan 13 Task 2.2 Step 3: a successful indexing pass must
        // record the runtime-supplied metadata to storage so the next
        // CLI status / MCP query can verify compatibility.
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::from_millis(0)));
        let embedder = std::sync::Arc::new(FakeEmbedder {
            sleep: std::time::Duration::from_millis(0),
        });
        let commit_source = FakeCommitSource {
            commits: vec![fake_commit("aaaa")],
            hunks: vec![fake_hunk("aaaa", "+x\n")],
            sleep_per_call: std::time::Duration::from_millis(0),
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::from_millis(0),
        };

        let indexer =
            Indexer::new(storage.clone(), embedder).with_runtime_metadata(fake_runtime_metadata());
        let repo_id = RepoId::from_parts("first", "/tmp/meta-success");
        indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("indexer run");

        let written = storage
            .last_metadata
            .lock()
            .unwrap()
            .clone()
            .expect("indexer must write runtime metadata on success");
        let by_key: std::collections::BTreeMap<&str, &str> = written
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(by_key.get("schema").copied(), Some("3"));
        assert_eq!(by_key.get("embedding_model").copied(), Some("fake-model"));
        assert_eq!(by_key.get("embedding_dimension").copied(), Some("4"));
        assert_eq!(by_key.get("chunker_version").copied(), Some("1"));
        assert_eq!(by_key.get("parser_rust").copied(), Some("1"));
    }

    #[tokio::test]
    async fn run_does_not_write_metadata_when_indexer_fails_partway() {
        // Plan 28 Task C.3: per-commit failure isolation means a broken
        // embedder no longer aborts the run. The run completes (with
        // new_commits == 0 for the skipped commit) and metadata IS
        // written because the run itself succeeded. This test now
        // validates that the metadata path is reached even when all
        // commits were individually skipped.
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::from_millis(0)));
        let embedder = std::sync::Arc::new(EmptyEmbedder);
        let commit_source = FakeCommitSource {
            commits: vec![fake_commit("aaaa")],
            hunks: vec![fake_hunk("aaaa", "+x\n")],
            sleep_per_call: std::time::Duration::from_millis(0),
        };
        let symbol_source = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::from_millis(0),
        };

        let indexer =
            Indexer::new(storage.clone(), embedder).with_runtime_metadata(fake_runtime_metadata());
        let repo_id = RepoId::from_parts("first", "/tmp/meta-failure");
        let report = indexer
            .run(&repo_id, Arc::new(commit_source), Arc::new(symbol_source))
            .await
            .expect("run must succeed with per-commit error isolation");

        assert_eq!(
            report.new_commits, 0,
            "broken embedder causes commit skip, not run failure"
        );
        // Metadata IS written because the run completed successfully.
        assert!(
            storage.last_metadata.lock().unwrap().is_some(),
            "metadata write MUST happen when the run succeeds (even with skipped commits)"
        );
    }

    #[tokio::test]
    async fn commit_exists_returns_true_for_seeded_sha_and_false_otherwise() {
        // Plan 9 Task 1.1: lock the trait shape — pre-seed the mock with
        // two SHAs (mimicking commits whose `commit_record` row was
        // written on a prior run) and verify the per-SHA lookup answers
        // membership cleanly. Drives the indexer's resume short-circuit
        // added in Plan 9 Task 2.1.
        let storage = FakeStorage::new(std::time::Duration::from_millis(0))
            .with_seen(["alpha".to_string(), "beta".to_string()]);

        assert!(
            storage.commit_exists("alpha").await.unwrap(),
            "seeded sha must report present"
        );
        assert!(
            storage.commit_exists("beta").await.unwrap(),
            "seeded sha must report present"
        );
        assert!(
            !storage.commit_exists("gamma").await.unwrap(),
            "unseen sha must report absent"
        );
    }

    /// Plan 15 Task B.1: when `with_embed_batch(N)` is set, the
    /// indexer must slice each commit's `embed_batch` input into
    /// chunks of at most N strings. Verifies (a) call count
    /// matches ceil(total_texts / N), (b) every chunk size is
    /// <= N. Reuses the module's existing `fake_commit` /
    /// `fake_hunk` helpers so the test doesn't drift from
    /// `CommitMeta` / `Hunk` field changes.
    #[tokio::test]
    async fn embed_batch_chunks_input_per_knob() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ChunkRecordingEmbedder {
            calls: std::sync::Arc<Mutex<Vec<usize>>>,
            #[allow(dead_code)]
            total: AtomicUsize,
        }

        #[async_trait]
        impl crate::EmbeddingProvider for ChunkRecordingEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "chunk-recorder"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.calls.lock().unwrap().push(texts.len());
                self.total.fetch_add(texts.len(), Ordering::SeqCst);
                Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
            }
        }

        // Five hunks + one commit message = six texts. With
        // embed_batch=2, expect chunks of [2, 2, 2].
        let hunks: Vec<Hunk> = (0..5)
            .map(|i| fake_hunk("deadbeef", &format!("+x{i}\n")))
            .collect();
        let cs = FakeCommitSource {
            commits: vec![fake_commit("deadbeef")],
            hunks: hunks.clone(),
            sleep_per_call: std::time::Duration::ZERO,
        };
        let ss = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        };
        let calls = std::sync::Arc::new(Mutex::new(Vec::<usize>::new()));
        let embedder = std::sync::Arc::new(ChunkRecordingEmbedder {
            calls: calls.clone(),
            total: AtomicUsize::new(0),
        });
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::ZERO));
        let indexer = Indexer::new(storage, embedder).with_embed_batch(2);
        let id = RepoId::from_parts("deadbeef", "/x");
        indexer.run(&id, Arc::new(cs), Arc::new(ss)).await.unwrap();

        let observed = calls.lock().unwrap().clone();
        assert_eq!(
            observed,
            vec![2, 2, 2],
            "expected three chunks of size 2, got {observed:?}"
        );
        for chunk in &observed {
            assert!(*chunk <= 2, "chunk size {chunk} exceeded knob");
        }
    }

    #[tokio::test]
    async fn embed_in_chunks_handles_empty_and_partial_final() {
        struct EchoEmbedder {
            calls: std::sync::Arc<Mutex<Vec<usize>>>,
        }
        #[async_trait]
        impl crate::EmbeddingProvider for EchoEmbedder {
            fn dimension(&self) -> usize {
                1
            }
            fn model_id(&self) -> &str {
                "echo"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.calls.lock().unwrap().push(texts.len());
                Ok(texts.iter().map(|_| vec![1.0_f32]).collect())
            }
        }

        let calls = std::sync::Arc::new(Mutex::new(Vec::<usize>::new()));
        let e = EchoEmbedder {
            calls: calls.clone(),
        };

        // Empty input -> zero calls, empty output.
        let out = super::embed_in_chunks(&e, &[], 4).await.unwrap();
        assert!(out.is_empty());
        assert!(calls.lock().unwrap().is_empty());

        // 7 texts with cap 3 -> chunks of [3, 3, 1].
        let texts: Vec<String> = (0..7).map(|i| format!("t{i}")).collect();
        let out = super::embed_in_chunks(&e, &texts, 3).await.unwrap();
        assert_eq!(out.len(), 7);
        assert_eq!(*calls.lock().unwrap(), vec![3, 3, 1]);
    }

    /// Plan 15 Task C.1: when `file_at_commit` returns a source
    /// larger than `MAX_ATTRIBUTABLE_SOURCE_BYTES`, the indexer
    /// must skip the atomic-symbol extraction path (which would
    /// otherwise build a full tree-sitter AST against the giant
    /// source) and fall back to the header-only attribution path.
    /// Verified by giving an extractor that PANICS if invoked, so
    /// any call into it fails the test loudly.
    #[tokio::test]
    async fn oversize_sources_skip_atomic_extraction() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PanicExtractor {
            calls: std::sync::Arc<AtomicUsize>,
        }
        impl crate::indexer::AtomicSymbolExtractor for PanicExtractor {
            fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("extractor must not be called for oversized sources");
            }
        }

        struct GiantSourceCommitSource {
            commits: Vec<CommitMeta>,
            hunks: Vec<Hunk>,
            source_bytes: usize,
        }
        #[async_trait]
        impl CommitSource for GiantSourceCommitSource {
            async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
                Ok(self.commits.clone())
            }
            async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
                Ok(self.hunks.clone())
            }
            async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>> {
                Ok(Some("x".repeat(self.source_bytes)))
            }
        }

        // Build the commit + hunk via the module's existing
        // helpers, then override the hunk's file_path so the
        // attribution code path tries to fetch a "big.js" source
        // (which the GiantSourceCommitSource always reports as
        // 4 MiB regardless of the path).
        let mut hunk = fake_hunk(
            "deadbeef",
            "--- a/vendor/big.js\n+++ b/vendor/big.js\n@@ -0,0 +1 @@\n+y\n",
        );
        hunk.file_path = "vendor/big.js".into();
        let cs = GiantSourceCommitSource {
            commits: vec![fake_commit("deadbeef")],
            hunks: vec![hunk],
            // 4 MiB — well over the 2 MiB default cap.
            source_bytes: 4 * 1024 * 1024,
        };
        let ss = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let extractor = std::sync::Arc::new(PanicExtractor {
            calls: calls.clone(),
        });

        struct ZeroEmbedder;
        #[async_trait]
        impl crate::EmbeddingProvider for ZeroEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "z"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
            }
        }

        let indexer = Indexer::new(
            std::sync::Arc::new(FakeStorage::new(std::time::Duration::ZERO)),
            std::sync::Arc::new(ZeroEmbedder),
        )
        .with_atomic_symbol_extractor(extractor);
        let id = RepoId::from_parts("deadbeef", "/x");
        indexer.run(&id, Arc::new(cs), Arc::new(ss)).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "extractor must not be called for oversized sources"
        );
    }

    #[tokio::test]
    async fn embed_in_chunks_preserves_per_element_ordering() {
        // Embedder returns a unique single-component vector per input
        // (the input's index). After chunked embedding we must recover
        // the same ordering — i.e. out[i] must be the embedding of
        // texts[i], not someone else's chunk.
        struct IndexedEmbedder;
        #[async_trait]
        impl crate::EmbeddingProvider for IndexedEmbedder {
            fn dimension(&self) -> usize {
                1
            }
            fn model_id(&self) -> &str {
                "indexed"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts
                    .iter()
                    .map(|t| vec![t.parse::<f32>().unwrap_or(-1.0)])
                    .collect())
            }
        }

        // texts[i] = "i", so out[i][0] must == i.
        let texts: Vec<String> = (0..7).map(|i| format!("{i}")).collect();
        let out = super::embed_in_chunks(&IndexedEmbedder, &texts, 3)
            .await
            .unwrap();
        assert_eq!(out.len(), 7);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(v.len(), 1);
            assert_eq!(v[0], i as f32, "out[{i}] should be embedding of texts[{i}]");
        }
    }

    /// Plan 15 Task C.1 sibling: when `file_at_commit` returns a source
    /// at or below `MAX_ATTRIBUTABLE_SOURCE_BYTES`, the indexer must
    /// still hit the ExactSpan extraction path. Verified by giving a
    /// counting extractor that records every call.
    #[tokio::test]
    async fn sub_cap_sources_still_hit_atomic_extraction() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingExtractor {
            calls: std::sync::Arc<AtomicUsize>,
        }
        impl crate::indexer::AtomicSymbolExtractor for CountingExtractor {
            fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }
        }

        struct SmallSourceCommitSource {
            commits: Vec<CommitMeta>,
            hunks: Vec<Hunk>,
            source_bytes: usize,
        }
        #[async_trait]
        impl CommitSource for SmallSourceCommitSource {
            async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
                Ok(self.commits.clone())
            }
            async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
                Ok(self.hunks.clone())
            }
            async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>> {
                Ok(Some("x".repeat(self.source_bytes)))
            }
        }

        struct ZeroEmbedder2;
        #[async_trait]
        impl crate::EmbeddingProvider for ZeroEmbedder2 {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "z"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
            }
        }

        let mut hunk = fake_hunk(
            "deadbeef",
            "--- a/src/small.rs\n+++ b/src/small.rs\n@@ -0,0 +1 @@\n+x\n",
        );
        hunk.file_path = "src/small.rs".into();
        // 1 KiB — well under the 2 MiB cap.
        let cs = SmallSourceCommitSource {
            commits: vec![fake_commit("deadbeef")],
            hunks: vec![hunk],
            source_bytes: 1024,
        };
        let ss = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let extractor = std::sync::Arc::new(CountingExtractor {
            calls: calls.clone(),
        });

        let indexer = Indexer::new(
            std::sync::Arc::new(FakeStorage::new(std::time::Duration::ZERO)),
            std::sync::Arc::new(ZeroEmbedder2),
        )
        .with_atomic_symbol_extractor(extractor);
        let id = RepoId::from_parts("deadbeef", "/x");
        indexer.run(&id, Arc::new(cs), Arc::new(ss)).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "extractor must be called exactly once for the in-bounds hunk"
        );
    }

    // ---- plan-33 Task 5: per-worktree tips ----

    use crate::worktree::{CommitSha, IndexedTip, TipStore, WorktreeKey};

    fn sha40(c: char) -> CommitSha {
        CommitSha::parse(&c.to_string().repeat(40)).expect("valid sha")
    }

    #[derive(Default)]
    struct FakeTips {
        tips: Mutex<Vec<IndexedTip>>,
    }

    impl FakeTips {
        fn with(tips: Vec<(WorktreeKey, CommitSha)>) -> Self {
            let tips = tips
                .into_iter()
                .map(|(worktree, sha)| IndexedTip { worktree, sha })
                .collect();
            Self {
                tips: Mutex::new(tips),
            }
        }
        fn snapshot(&self) -> Vec<IndexedTip> {
            self.tips.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TipStore for FakeTips {
        async fn indexed_tips(&self, _: &RepoId) -> Result<Vec<IndexedTip>> {
            Ok(self.snapshot())
        }
        async fn set_indexed_tip(
            &self,
            _: &RepoId,
            worktree: &WorktreeKey,
            sha: &CommitSha,
        ) -> Result<()> {
            let mut tips = self.tips.lock().unwrap();
            tips.retain(|t| &t.worktree != worktree);
            tips.push(IndexedTip {
                worktree: worktree.clone(),
                sha: sha.clone(),
            });
            Ok(())
        }
        async fn remove_indexed_tip(
            &self,
            _: &RepoId,
            _: &WorktreeKey,
            _: &CommitSha,
        ) -> Result<bool> {
            Ok(false)
        }
        async fn replace_indexed_tip(
            &self,
            _: &RepoId,
            _: &WorktreeKey,
            _: &CommitSha,
            _: &CommitSha,
        ) -> Result<bool> {
            Ok(false)
        }
        async fn indexed_commit_shas(&self, _: &RepoId) -> Result<Vec<CommitSha>> {
            Ok(vec![])
        }
        async fn delete_commits(&self, _: &RepoId, _: &[CommitSha]) -> Result<u64> {
            Ok(0)
        }
    }

    /// Records the `from` / `hide` arguments of the hiding walk; can poison one sha.
    struct HidingSource {
        commits: Vec<CommitMeta>,
        poison: Option<String>,
        seen_hide: Mutex<Option<Vec<String>>>,
        seen_from: Mutex<Option<String>>,
    }

    impl HidingSource {
        fn new(commits: Vec<CommitMeta>) -> Self {
            Self {
                commits,
                poison: None,
                seen_hide: Mutex::new(None),
                seen_from: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl CommitSource for HidingSource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
            Err(crate::OhraError::Git(
                "legacy list_commits must not run".into(),
            ))
        }
        async fn list_commits_hiding(&self, hide: &[String]) -> Result<Vec<CommitMeta>> {
            *self.seen_hide.lock().unwrap() = Some(hide.to_vec());
            Ok(self.commits.clone())
        }
        async fn list_commits_from_hiding(
            &self,
            from: &str,
            hide: &[String],
        ) -> Result<Vec<CommitMeta>> {
            *self.seen_from.lock().unwrap() = Some(from.to_string());
            self.list_commits_hiding(hide).await
        }
        async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>> {
            if self.poison.as_deref() == Some(sha) {
                return Err(crate::OhraError::Git(format!("poisoned {sha}")));
            }
            Ok(vec![fake_hunk(sha, "+x\n")])
        }
    }

    /// Panics if HEAD symbols are requested.
    struct PanickingSymbolSource;

    #[async_trait]
    impl SymbolSource for PanickingSymbolSource {
        async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
            panic!("extract_head_symbols must not be called once main has a tip");
        }
    }

    /// Every commit is an ancestor of every head: no tip is ever retired.
    struct AllVisible;

    #[async_trait]
    impl crate::visibility::CommitVisibility for AllVisible {
        async fn visible(&self, _: &CommitSha, commits: &[CommitSha]) -> Result<Vec<bool>> {
            Ok(vec![true; commits.len()])
        }
    }

    fn tip_indexer(storage: Arc<FakeStorage>, tips: Arc<FakeTips>, wt: WorktreeKey) -> Indexer {
        Indexer::new(
            storage,
            std::sync::Arc::new(FakeEmbedder {
                sleep: std::time::Duration::ZERO,
            }),
        )
        .with_tip_context(TipContext {
            tips,
            visibility: Arc::new(AllVisible),
            worktree: wt,
            head: sha40('f'),
        })
    }

    fn zero_storage() -> Arc<FakeStorage> {
        Arc::new(FakeStorage::new(std::time::Duration::ZERO))
    }

    fn no_symbols() -> Arc<FakeSymbolSource> {
        Arc::new(FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        })
    }

    #[tokio::test]
    async fn legacy_watermark_does_not_advance_past_failed_commits() {
        let storage = zero_storage();
        let source = WatermarkOnlySource { poison: Some("c1") };
        let indexer = Indexer::new(
            storage.clone(),
            std::sync::Arc::new(FakeEmbedder {
                sleep: std::time::Duration::ZERO,
            }),
        );
        let repo_id = RepoId::from_parts("first", "/tmp/legacy-failed");
        let report = indexer
            .run(&repo_id, Arc::new(source), no_symbols())
            .await
            .expect("run");
        assert_eq!(report.commits_failed, 1);
        assert_eq!(
            storage.last_indexed.lock().unwrap().clone(),
            None,
            "a run with failed commits must leave the watermark alone"
        );
    }

    /// Implements only the required `CommitSource` methods; can poison one sha.
    struct WatermarkOnlySource {
        poison: Option<&'static str>,
    }

    #[async_trait]
    impl CommitSource for WatermarkOnlySource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
            Ok(vec![fake_commit("c1"), fake_commit("c2")])
        }
        async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>> {
            if self.poison == Some(sha) {
                return Err(crate::OhraError::Git(format!("poisoned {sha}")));
            }
            Ok(vec![fake_hunk(sha, "+x\n")])
        }
    }

    #[tokio::test]
    async fn default_hiding_walks_refuse_what_they_cannot_honour() {
        let s = WatermarkOnlySource { poison: None };
        assert!(s.list_commits_hiding(&[]).await.is_ok());
        assert!(s.list_commits_hiding(&["a".repeat(40)]).await.is_ok());
        assert!(
            s.list_commits_hiding(&["a".repeat(40), "b".repeat(40)])
                .await
                .is_err(),
            "a second hide tip must not be silently dropped"
        );
        assert!(
            s.list_commits_from_hiding(&"f".repeat(40), &[])
                .await
                .is_err(),
            "an explicit start commit must not be silently replaced by HEAD"
        );
    }

    #[tokio::test]
    async fn indexer_hides_every_indexed_tip_and_records_the_worktree_head() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(FakeTips::with(vec![
            (WorktreeKey::main(), sha40('a')),
            (WorktreeKey::linked("slot-2").unwrap(), sha40('b')),
        ]));
        let storage = zero_storage();
        let source = Arc::new(HidingSource::new(vec![fake_commit("c1")]));
        let indexer = tip_indexer(storage.clone(), tips.clone(), slot1.clone());
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        indexer
            .run(&repo_id, source.clone(), Arc::new(PanickingSymbolSource))
            .await
            .expect("run");

        let from = source.seen_from.lock().unwrap().clone();
        assert_eq!(from, Some("f".repeat(40)), "walk must start at ctx.head");
        let mut hidden = source.seen_hide.lock().unwrap().clone().expect("hide seen");
        hidden.sort();
        assert_eq!(hidden, vec!["a".repeat(40), "b".repeat(40)]);
        let snap = tips.snapshot();
        assert!(snap.contains(&IndexedTip {
            worktree: slot1,
            sha: sha40('f')
        }));
        assert!(snap.contains(&IndexedTip {
            worktree: WorktreeKey::main(),
            sha: sha40('a')
        }));
        assert!(
            storage.last_indexed.lock().unwrap().is_none(),
            "legacy watermark must not be written when a TipContext is set"
        );
    }

    #[tokio::test]
    async fn linked_worktree_run_never_touches_head_symbols() {
        let tips = Arc::new(FakeTips::with(vec![(WorktreeKey::main(), sha40('a'))]));
        let storage = zero_storage();
        let indexer = tip_indexer(
            storage.clone(),
            tips,
            WorktreeKey::linked("slot-1").unwrap(),
        );
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        let report = indexer
            .run(
                &repo_id,
                Arc::new(HidingSource::new(vec![fake_commit("c1")])),
                Arc::new(PanickingSymbolSource),
            )
            .await
            .expect("run");
        assert_eq!(report.head_symbols, 0);
        assert_eq!(*storage.head_puts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn linked_worktree_writes_head_symbols_while_main_has_no_tip() {
        let tips = Arc::new(FakeTips::with(vec![(
            WorktreeKey::linked("slot-2").unwrap(),
            sha40('b'),
        )]));
        let storage = zero_storage();
        let indexer = tip_indexer(
            storage.clone(),
            tips,
            WorktreeKey::linked("slot-1").unwrap(),
        );
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        indexer
            .run(
                &repo_id,
                Arc::new(HidingSource::new(vec![fake_commit("c1")])),
                no_symbols(),
            )
            .await
            .expect("run");
        assert_eq!(*storage.head_puts.lock().unwrap(), 1);
    }

    /// Fails HEAD-symbol extraction.
    struct FailingSymbolSource;

    #[async_trait]
    impl SymbolSource for FailingSymbolSource {
        async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
            Err(crate::OhraError::Git("extraction failed".into()))
        }
    }

    fn named_symbol(name: &str) -> Symbol {
        Symbol {
            file_path: "src/a.rs".into(),
            language: "rust".into(),
            kind: crate::types::SymbolKind::Function,
            name: name.into(),
            qualified_name: None,
            sibling_names: Vec::new(),
            span_start: 0,
            span_end: 1,
            blob_sha: "b".into(),
            source_text: format!("fn {name}() {{}}"),
        }
    }

    #[tokio::test]
    async fn failed_head_symbol_extraction_keeps_the_previous_snapshot() {
        // Main's first run (a linked worktree wrote the snapshot) used to
        // clear it before extracting; a failed extraction left it empty.
        let tips = Arc::new(FakeTips::with(vec![(
            WorktreeKey::linked("slot-1").unwrap(),
            sha40('b'),
        )]));
        let storage = zero_storage();
        *storage.head_snapshot.lock().unwrap() = vec![named_symbol("kept")];
        let indexer = tip_indexer(storage.clone(), tips, WorktreeKey::main());
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        let res = indexer
            .run(
                &repo_id,
                Arc::new(HidingSource::new(vec![])),
                Arc::new(FailingSymbolSource),
            )
            .await;
        assert!(res.is_err(), "a failed extraction fails the run");
        let names: Vec<String> = storage
            .head_snapshot
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["kept".to_string()]);
    }

    #[tokio::test]
    async fn empty_walk_still_records_the_tip() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(FakeTips::default());
        let indexer = tip_indexer(zero_storage(), tips.clone(), slot1.clone());
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        indexer
            .run(&repo_id, Arc::new(HidingSource::new(vec![])), no_symbols())
            .await
            .expect("run");
        assert_eq!(
            tips.snapshot(),
            vec![IndexedTip {
                worktree: slot1,
                sha: sha40('f')
            }]
        );
    }

    #[tokio::test]
    async fn failed_commits_leave_the_tip_unset() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(FakeTips::default());
        let mut source = HidingSource::new(vec![fake_commit("c1"), fake_commit("c2")]);
        source.poison = Some("c2".into());
        let indexer = tip_indexer(zero_storage(), tips.clone(), slot1);
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        let report = indexer
            .run(&repo_id, Arc::new(source), no_symbols())
            .await
            .expect("run");
        assert_eq!(report.commits_failed, 1);
        assert!(tips.snapshot().is_empty(), "tip must stay unset");
    }

    #[tokio::test]
    async fn without_tip_context_the_legacy_watermark_path_is_unchanged() {
        let storage = zero_storage();
        let indexer = Indexer::new(
            storage.clone(),
            std::sync::Arc::new(FakeEmbedder {
                sleep: std::time::Duration::ZERO,
            }),
        );
        let source = FakeCommitSource {
            commits: vec![fake_commit("aaaa")],
            hunks: vec![fake_hunk("aaaa", "+x\n")],
            sleep_per_call: std::time::Duration::ZERO,
        };
        let repo_id = RepoId::from_parts("first", "/tmp/r");
        indexer
            .run(&repo_id, Arc::new(source), no_symbols())
            .await
            .expect("run");
        assert_eq!(
            storage.last_indexed.lock().unwrap().as_deref(),
            Some("aaaa")
        );
        assert_eq!(*storage.head_puts.lock().unwrap(), 1);
    }
}

#[cfg(test)]
mod plan_26_repo_root_tests {
    use super::*;

    #[test]
    fn indexer_with_repo_root_stores_path() {
        // Plan 26 Task C.4: builder stores the repo root.
        // Uses the same fake storage/embedder pattern as phase_timing_tests.
        use crate::query::IndexStatus;
        use crate::storage::{CommitRecord, HunkRecord};
        use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
        use async_trait::async_trait;
        use std::collections::HashSet;
        use std::sync::Mutex;

        struct MinStorage;

        #[async_trait]
        impl crate::storage::StorageStatus for MinStorage {
            async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> {
                Ok(IndexStatus {
                    last_indexed_commit: None,
                    commits_behind_head: 0,
                    indexed_at: None,
                })
            }

            async fn get_index_metadata(
                &self,
                _: &RepoId,
            ) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
                Ok(crate::index_metadata::StoredIndexMetadata::default())
            }

            async fn put_index_metadata(
                &self,
                _: &RepoId,
                _: &[(String, String)],
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        #[async_trait]
        impl crate::storage::StorageWrite for MinStorage {
            async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> {
                Ok(())
            }

            async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> {
                Ok(())
            }

            async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> {
                Ok(())
            }

            async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
                Ok(false)
            }

            async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> {
                Ok(())
            }

            async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> {
                Ok(())
            }
        }

        #[async_trait]
        impl crate::storage::StorageRead for MinStorage {
            async fn knn_hunks(
                &self,
                _: &RepoId,
                _: &[f32],
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_text(
                &self,
                _: &RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_semantic_text(
                &self,
                _: &RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_symbol_name(
                &self,
                _: &RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_historical_symbol(
                &self,
                _: &RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn get_hunk_symbols(
                &self,
                _: &RepoId,
                _: crate::storage::HunkId,
            ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
                Ok(vec![])
            }

            async fn get_hunk_symbols_batch(
                &self,
                _: &RepoId,
                _: &[crate::storage::HunkId],
            ) -> crate::Result<
                std::collections::HashMap<crate::storage::HunkId, Vec<crate::types::HunkSymbol>>,
            > {
                Ok(std::collections::HashMap::new())
            }
        }

        #[async_trait]
        impl crate::storage::StorageExplain for MinStorage {
            async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<CommitMeta>> {
                Ok(None)
            }

            async fn get_hunks_for_file_in_commit(
                &self,
                _: &RepoId,
                _: &str,
                _: &str,
            ) -> crate::Result<Vec<Hunk>> {
                Ok(vec![])
            }

            async fn get_neighboring_file_commits(
                &self,
                _: &RepoId,
                _: &str,
                _: &str,
                _: u8,
                _: u8,
            ) -> crate::Result<Vec<(u32, CommitMeta)>> {
                Ok(vec![])
            }
        }

        #[async_trait]
        impl crate::storage::StorageCache for MinStorage {
            async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
                Ok(false)
            }

            async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
                Ok(())
            }
        }

        struct MinEmbedder;

        #[async_trait]
        impl crate::EmbeddingProvider for MinEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "min"
            }
            async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
            }
        }

        let _ = (HashSet::<String>::new(), Mutex::new(()));

        let i = Indexer::new(
            std::sync::Arc::new(MinStorage),
            std::sync::Arc::new(MinEmbedder),
        )
        .with_repo_root(std::path::PathBuf::from("/tmp/example"));
        assert_eq!(
            i.repo_root.as_deref(),
            Some(std::path::Path::new("/tmp/example"))
        );
    }
}

#[cfg(test)]
mod plan_28_workers_tests {
    use super::*;

    #[test]
    fn indexer_with_workers_clamps_to_one_minimum() {
        // Plan 28 Task C.1: with_workers(0) is normalised to 1.
        struct MinStorage;

        #[async_trait::async_trait]
        impl crate::storage::StorageStatus for MinStorage {
            async fn get_index_status(
                &self,
                _: &crate::types::RepoId,
            ) -> crate::Result<crate::query::IndexStatus> {
                Ok(crate::query::IndexStatus {
                    last_indexed_commit: None,
                    commits_behind_head: 0,
                    indexed_at: None,
                })
            }

            async fn get_index_metadata(
                &self,
                _: &crate::types::RepoId,
            ) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
                Ok(crate::index_metadata::StoredIndexMetadata::default())
            }

            async fn put_index_metadata(
                &self,
                _: &crate::types::RepoId,
                _: &[(String, String)],
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        #[async_trait::async_trait]
        impl crate::storage::StorageWrite for MinStorage {
            async fn open_repo(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: &str,
            ) -> crate::Result<()> {
                Ok(())
            }

            async fn set_last_indexed_commit(
                &self,
                _: &crate::types::RepoId,
                _: &str,
            ) -> crate::Result<()> {
                Ok(())
            }

            async fn put_commit(
                &self,
                _: &crate::types::RepoId,
                _: &crate::storage::CommitRecord,
            ) -> crate::Result<()> {
                Ok(())
            }

            async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
                Ok(false)
            }

            async fn put_hunks(
                &self,
                _: &crate::types::RepoId,
                _: &[crate::storage::HunkRecord],
            ) -> crate::Result<()> {
                Ok(())
            }

            async fn put_head_symbols(
                &self,
                _: &crate::types::RepoId,
                _: &[crate::types::Symbol],
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        #[async_trait::async_trait]
        impl crate::storage::StorageRead for MinStorage {
            async fn knn_hunks(
                &self,
                _: &crate::types::RepoId,
                _: &[f32],
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_text(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_semantic_text(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_symbol_name(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn bm25_hunks_by_historical_symbol(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: u8,
                _: Option<&str>,
                _: Option<i64>,
            ) -> crate::Result<Vec<crate::HunkHit>> {
                Ok(vec![])
            }

            async fn get_hunk_symbols(
                &self,
                _: &crate::types::RepoId,
                _: crate::storage::HunkId,
            ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
                Ok(vec![])
            }

            async fn get_hunk_symbols_batch(
                &self,
                _: &crate::types::RepoId,
                _: &[crate::storage::HunkId],
            ) -> crate::Result<
                std::collections::HashMap<crate::storage::HunkId, Vec<crate::types::HunkSymbol>>,
            > {
                Ok(std::collections::HashMap::new())
            }
        }

        #[async_trait::async_trait]
        impl crate::storage::StorageExplain for MinStorage {
            async fn get_commit(
                &self,
                _: &crate::types::RepoId,
                _: &str,
            ) -> crate::Result<Option<crate::types::CommitMeta>> {
                Ok(None)
            }

            async fn get_hunks_for_file_in_commit(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: &str,
            ) -> crate::Result<Vec<crate::types::Hunk>> {
                Ok(vec![])
            }

            async fn get_neighboring_file_commits(
                &self,
                _: &crate::types::RepoId,
                _: &str,
                _: &str,
                _: u8,
                _: u8,
            ) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> {
                Ok(vec![])
            }
        }

        #[async_trait::async_trait]
        impl crate::storage::StorageCache for MinStorage {
            async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
                Ok(false)
            }

            async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
                Ok(())
            }
        }

        struct MinEmbedder;

        #[async_trait::async_trait]
        impl crate::EmbeddingProvider for MinEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "min"
            }
            async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
            }
        }

        let i = Indexer::new(
            std::sync::Arc::new(MinStorage),
            std::sync::Arc::new(MinEmbedder),
        )
        .with_workers(0);
        assert_eq!(i.workers, 1, "with_workers(0) must be clamped to 1");

        let j = Indexer::new(
            std::sync::Arc::new(MinStorage),
            std::sync::Arc::new(MinEmbedder),
        )
        .with_workers(4);
        assert_eq!(j.workers, 4, "with_workers(4) must be stored as 4");
    }
}

#[cfg(test)]
mod stage_type_tests {
    #[test]
    fn stage_types_compose_into_pipeline_chain() {
        use crate::stages::{AttributedHunk, EmbeddedHunk, HunkRecord};
        use crate::types::{ChangeKind, Hunk};

        // Verify the chain compiles and the helper methods are reachable.
        let hunk = Hunk {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            language: None,
            change_kind: ChangeKind::Added,
            diff_text: "+fn foo() {}\n".into(),
        };
        let record = HunkRecord {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn foo() {}\n".into(),
            semantic_text: "fn foo() {}".into(),
            source_hunk: hunk,
        };
        let attributed = AttributedHunk {
            record,
            symbols: None,
            attributed_semantic_text: None,
        };
        assert_eq!(attributed.effective_semantic_text(), "fn foo() {}");
        let embedded = EmbeddedHunk {
            attributed,
            embedding: vec![0.1, 0.2, 0.3, 0.4],
        };
        assert_eq!(embedded.embedding.len(), 4);
        let _ = embedded; // consumed — verifies ownership model
    }
}
