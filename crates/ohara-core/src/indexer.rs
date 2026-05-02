use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, OhraError, Result, Storage};
use std::sync::Arc;
use std::time::Instant;

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

                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(hunks.iter().map(|h| h.diff_text.clone()))
                    .collect();
                let embed_start = Instant::now();
                let embs = self.embedder.embed_batch(&texts).await?;
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

                let records: Vec<HunkRecord> = hunks
                    .into_iter()
                    .zip(hunk_embs.iter().cloned())
                    .map(|(h, e)| HunkRecord {
                        hunk: h,
                        diff_emb: e,
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
    }

    impl FakeStorage {
        fn new(write_sleep: std::time::Duration) -> Self {
            Self {
                write_sleep,
                last_indexed: Mutex::new(None),
                seen_commits: Mutex::new(HashSet::new()),
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
        async fn get_index_metadata(
            &self,
            _: &RepoId,
        ) -> Result<crate::index_metadata::StoredIndexMetadata> {
            Ok(crate::index_metadata::StoredIndexMetadata::default())
        }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> {
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
                assert!(
                    msg.contains("embed_batch returned empty"),
                    "expected embed_batch-empty diagnostic, got: {msg}"
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
}
