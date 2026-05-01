use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
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
                let extract_start = Instant::now();
                let hunks = commit_source.hunks_for_commit(&cm.sha).await?;
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
                let (msg_emb, hunk_embs) = embs.split_first().expect("non-empty");

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

#[derive(Debug, Clone)]
pub struct IndexerReport {
    pub new_commits: usize,
    pub new_hunks: usize,
    pub head_symbols: usize,
    /// Per-phase wall-time + input-size breakdown for the run, captured
    /// when the `--profile` flag (or any caller plumbing
    /// `Indexer::run`) wants the same numbers used for v0.6 throughput
    /// analysis. Always present so consumers don't branch on
    /// `Option`; fields default to zero on a no-op pass.
    pub phase_timings: PhaseTimings,
}

/// Per-phase wall-time + input-size breakdown for one `Indexer::run`
/// invocation. Field semantics are cumulative across the commit walk:
/// `embed_ms` is the sum of every per-commit `embed_batch` call, etc.
/// `total_diff_bytes / total_added_lines` gives the hunk-text inflation
/// ratio called out in the v0.6 RFC (high ratio ⇒ trim git2 context).
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
    /// `bytes_per_added_line` — the cheap-win measurement from the
    /// v0.6 RFC (Task 0.3).
    pub total_diff_bytes: u64,
    /// Sum of every "+"-prefixed line count across all hunks during
    /// the run. Counts only added lines (context / removed lines are
    /// excluded) so the resulting ratio reflects "bytes the embedder
    /// sees per signal-bearing line".
    pub total_added_lines: u64,
}

#[cfg(test)]
mod phase_timing_tests {
    use super::*;
    use crate::query::IndexStatus;
    use crate::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol};
    use async_trait::async_trait;
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
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> Result<()> {
            std::thread::sleep(self.write_sleep);
            Ok(())
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
    }

    fn fake_commit(sha: &str) -> CommitMeta {
        CommitMeta {
            sha: sha.into(),
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
        let storage = std::sync::Arc::new(FakeStorage {
            write_sleep: sleep,
            last_indexed: Mutex::new(None),
        });
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
}
