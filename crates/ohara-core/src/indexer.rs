pub mod stages;

use crate::index_metadata::RuntimeIndexMetadata;
use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, OhraError, Result, Storage};
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
    embed_batch: usize,
    progress: Arc<dyn ProgressSink>,
    /// Plan 13: optional runtime metadata recorded at the end of a
    /// successful index pass. Left unset by tests that don't care about
    /// compatibility tracking; the CLI / MCP wire it from
    /// `RuntimeIndexMetadata::current(...)`.
    runtime_metadata: Option<RuntimeIndexMetadata>,
    /// Plan 11: per-file atomic-symbol extractor used to derive
    /// ExactSpan hunk-symbol attribution. Defaults to the
    /// header-only `NullAtomicSymbolExtractor` so legacy callers and
    /// test fakes don't have to wire tree-sitter just to run the
    /// indexer.
    symbol_extractor: Arc<dyn AtomicSymbolExtractor>,
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

    /// Run a (full or incremental) indexing pass for `repo_id`.
    /// `commit_source` and `symbol_source` are wired by the caller.
    pub async fn run(
        &self,
        repo_id: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<IndexerReport> {
        let mut timings = PhaseTimings::default();

        let status = self.storage.get_index_status(repo_id).await?;
        let walk_start = Instant::now();
        let commits = commit_source
            .list_commits(status.last_indexed_commit.as_deref())
            .await?;
        timings.commit_walk_ms = walk_start.elapsed().as_millis() as u64;
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
                // Plan 9: resume short-circuit. The watermark only excludes
                // strict ancestors of the last-indexed sha; commits reachable
                // via a different parent path (merge from a feature branch,
                // octopus merge, history rewrite) would otherwise be re-walked
                // and re-embedded even though their commit_record row already
                // exists. A sub-millisecond PK lookup avoids 14+ minutes of
                // wasted embedding on merge-heavy resumes.
                if self.storage.commit_exists(&cm.commit_sha).await? {
                    tracing::debug!(sha = %cm.commit_sha, "skip already-indexed commit");
                    latest_sha = Some(cm.commit_sha.clone());
                    commits_done += 1;
                    self.progress.commit_done(commits_done, total_hunks);
                    if commits_done % PROGRESS_INTERVAL == 0 {
                        if let Some(sha) = latest_sha.as_deref() {
                            self.storage.set_last_indexed_commit(repo_id, sha).await?;
                        }
                    }
                    continue;
                }
                let extract_start = Instant::now();
                let hunks = commit_source.hunks_for_commit(&cm.commit_sha).await?;
                timings.diff_extract_ms += extract_start.elapsed().as_millis() as u64;
                total_hunks += hunks.len();

                // Hunk-text inflation accounting (Task 0.3): the
                // embedder sees `diff_text` byte-for-byte, so summing
                // its byte-length gives the numerator. Added-line
                // count is the signal-bearing denominator — context
                // lines, deletions, and the `@@`/`---`/`+++` headers
                // are excluded so the resulting ratio reflects "bytes
                // per line that actually changed".
                for h in &hunks {
                    timings.total_diff_bytes += h.diff_text.len() as u64;
                    timings.total_added_lines += count_added_lines(&h.diff_text);
                }

                // Plan 11 Task 3.1: per-hunk symbol attribution.
                // Caller-side note: file_at_commit defaults to Ok(None)
                // on the trait, so test fakes that don't override it
                // produce HunkHeader-only attribution rather than
                // erroring. The attribution callback is the single
                // place that decides which symbols a hunk touched —
                // the indexer just stores the result.
                let mut hunk_attributions: Vec<Vec<crate::types::HunkSymbol>> =
                    Vec::with_capacity(hunks.len());
                for h in &hunks {
                    let source_opt = commit_source
                        .file_at_commit(&cm.commit_sha, &h.file_path)
                        .await?;
                    let attribution = match source_opt {
                        Some(source) if source.len() <= MAX_ATTRIBUTABLE_SOURCE_BYTES => {
                            // ExactSpan path: extract atomic symbols
                            // from the post-image source and intersect
                            // their line spans against the hunk's
                            // @@-headers.
                            let atoms = self.symbol_extractor.extract(&h.file_path, &source);
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: Some(&atoms),
                                source: Some(&source),
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                        Some(source) => {
                            tracing::debug!(
                                file = %h.file_path,
                                size = source.len(),
                                "skipping ExactSpan attribution for oversized source"
                            );
                            // Header-only path. `source` goes out of
                            // scope at the end of this arm.
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: None,
                                source: None,
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                        None => {
                            // file_at_commit reported absence (deleted,
                            // renamed-away, binary).
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: None,
                                source: None,
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                    };
                    hunk_attributions.push(attribution);
                }

                // Plan 11 Task 2.1: build the semantic-text
                // representation up front. Now also feeds the
                // symbols list from the attribution step above.
                // Step 4 fallback: when the builder's added_lines
                // section is empty (deletion-only hunk, etc.), fall
                // back to raw diff_text so the embedder still sees
                // the change rather than an empty string.
                let semantic_texts: Vec<String> = hunks
                    .iter()
                    .zip(hunk_attributions.iter())
                    .map(|(h, syms)| {
                        let body = crate::hunk_text::build(h, &cm.message, syms);
                        if body.contains("added_lines:") {
                            body
                        } else {
                            h.diff_text.clone()
                        }
                    })
                    .collect();
                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(semantic_texts.iter().cloned())
                    .collect();
                let embed_start = Instant::now();
                let embs =
                    embed_in_chunks(self.embedder.as_ref(), &texts, self.embed_batch).await?;
                timings.embed_ms += embed_start.elapsed().as_millis() as u64;
                // `texts` always contains at least 1 element (the commit
                // message), so an empty `embs` here is an embedder bug, not
                // an invariant the caller can violate. Surface it as a
                // typed error rather than panicking so the indexer still
                // reports a clean OhraError to its caller.
                let (msg_emb, hunk_embs) = match embs.split_first() {
                    Some(pair) => pair,
                    None => {
                        return Err(OhraError::Embedding(
                            "embed_batch returned empty for non-empty input".into(),
                        ));
                    }
                };

                let write_start = Instant::now();
                self.storage
                    .put_commit(
                        repo_id,
                        &CommitRecord {
                            meta: cm.clone(),
                            message_emb: msg_emb.clone(),
                        },
                    )
                    .await?;

                // Plan 11 Task 2.1 + 3.1: persist raw diff (display /
                // provenance), semantic_text (search-time), and
                // per-hunk symbol attribution computed above.
                let records: Vec<HunkRecord> = hunks
                    .into_iter()
                    .zip(hunk_embs.iter().cloned())
                    .zip(semantic_texts)
                    .zip(hunk_attributions)
                    .map(|(((h, e), semantic_text), symbols)| HunkRecord {
                        hunk: h,
                        diff_emb: e,
                        semantic_text,
                        symbols,
                    })
                    .collect();
                self.storage.put_hunks(repo_id, &records).await?;
                timings.storage_write_ms += write_start.elapsed().as_millis() as u64;
                latest_sha = Some(cm.commit_sha.clone());
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
        let symbols_start = Instant::now();
        let symbols = symbol_source.extract_head_symbols().await?;
        self.storage.put_head_symbols(repo_id, &symbols).await?;
        timings.head_symbols_ms = symbols_start.elapsed().as_millis() as u64;

        if let Some(sha) = latest_sha.as_deref() {
            self.storage.set_last_indexed_commit(repo_id, sha).await?;
        }

        // Plan 13: write runtime metadata only on full success — after
        // hunks, HEAD symbols, and the final watermark are persisted.
        // A failure earlier in the pass propagates as `?` above, which
        // means we never reach this point and the previously-recorded
        // metadata is preserved (so a half-finished run can't claim
        // the new version is complete).
        if let Some(meta) = &self.runtime_metadata {
            self.storage
                .put_index_metadata(repo_id, &meta.to_storage_components())
                .await?;
        }

        self.progress
            .finish(total_commits, total_hunks, symbols.len());
        Ok(IndexerReport {
            new_commits: commits.len(),
            new_hunks: total_hunks,
            head_symbols: symbols.len(),
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
            return Err(OhraError::Embedding(format!(
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
    /// (`Storage::put_commit` + `Storage::put_hunks`). Combined because
    /// they share a transaction in the SQLite backend.
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
    }

    impl FakeStorage {
        fn new(write_sleep: std::time::Duration) -> Self {
            Self {
                write_sleep,
                last_indexed: Mutex::new(None),
                seen_commits: Mutex::new(HashSet::new()),
                last_metadata: Mutex::new(None),
            }
        }

        fn with_seen<I: IntoIterator<Item = String>>(self, shas: I) -> Self {
            self.seen_commits.lock().unwrap().extend(shas);
            self
        }
    }

    #[async_trait]
    impl crate::Storage for FakeStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
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
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> {
            std::thread::sleep(self.write_sleep);
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> Result<()> {
            Ok(())
        }
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
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
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
            .run(&repo_id, &commit_source, &symbol_source)
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
        // Replaces the previous `.expect("non-empty")` panic with a
        // typed OhraError::Embedding so a buggy embedder surfaces as a
        // clean error to the caller instead of crashing the indexer.
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
        let err = indexer
            .run(&repo_id, &commit_source, &symbol_source)
            .await
            .expect_err("buggy embedder must surface an OhraError, not panic");
        match err {
            OhraError::Embedding(msg) => {
                // embed_in_chunks catches the mismatch (0 vectors for N
                // inputs) before split_first sees an empty Vec, so the
                // error now says "embed_batch returned 0 vectors for …"
                // rather than the previous "embed_batch returned empty".
                // Both are sub-strings of "embed_batch returned".
                assert!(
                    msg.contains("embed_batch returned") && msg.contains("vectors for"),
                    "expected embed_batch length-mismatch diagnostic, got: {msg}"
                );
            }
            other => panic!("expected OhraError::Embedding, got {other:?}"),
        }
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
            .run(&repo_id, &commit_source, &symbol_source)
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
            .run(&repo_id, &commit_source, &symbol_source)
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
            .run(&repo_id, &commit_source, &symbol_source)
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
        // Plan 13 Task 2.2 Step 4: if a pass fails before reaching the
        // metadata-write step (here: the embedder returns empty for
        // non-empty input — same path the existing typed-error test
        // exercises), storage MUST NOT see a put_index_metadata call.
        // That keeps a half-finished run from claiming the new
        // version is complete.
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
        let _err = indexer
            .run(&repo_id, &commit_source, &symbol_source)
            .await
            .expect_err("indexer must surface the embedder error");

        assert!(
            storage.last_metadata.lock().unwrap().is_none(),
            "metadata write must NOT happen when the pass fails partway"
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
        indexer.run(&id, &cs, &ss).await.unwrap();

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
        indexer.run(&id, &cs, &ss).await.unwrap();
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
        indexer.run(&id, &cs, &ss).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "extractor must be called exactly once for the in-bounds hunk"
        );
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
