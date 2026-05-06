//! Unit tests for the coordinator pipeline.

use super::*;
use crate::index_metadata::StoredIndexMetadata;
use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkHit, HunkId, HunkRecord as StorageHunkRecord};
use crate::types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

// --- Minimal fakes reused across coordinator tests ---

struct SingleCommitSource {
    sha: String,
    hunks: Vec<Hunk>,
}

#[async_trait]
impl crate::indexer::CommitSource for SingleCommitSource {
    async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
        Ok(vec![CommitMeta {
            commit_sha: self.sha.clone(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_000_000,
            message: "add feature".into(),
        }])
    }
    async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
        Ok(self.hunks.clone())
    }
}

struct NoopSymbolSource;
#[async_trait]
impl crate::indexer::SymbolSource for NoopSymbolSource {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
        Ok(vec![])
    }
}

struct ZeroEmbedder {
    dim: usize,
}
#[async_trait]
impl EmbeddingProvider for ZeroEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        "zero"
    }
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }
}

#[derive(Default)]
struct SpyStorage {
    put_commit_calls: Mutex<Vec<String>>,
    put_hunk_totals: Mutex<Vec<usize>>,
    /// Every `file_path` from every `put_hunks` call, in order.
    put_hunk_paths: Mutex<Vec<String>>,
    watermark: Mutex<Option<String>>,
    seen_commits: Mutex<Vec<String>>,
}

#[async_trait]
impl Storage for SpyStorage {
    async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
        Ok(IndexStatus {
            last_indexed_commit: self.watermark.lock().unwrap().clone(),
            commits_behind_head: 0,
            indexed_at: None,
        })
    }
    async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> Result<()> {
        Ok(())
    }
    async fn put_commit(&self, _: &RepoId, meta: &CommitRecord) -> Result<()> {
        self.put_commit_calls
            .lock()
            .unwrap()
            .push(meta.meta.commit_sha.clone());
        self.seen_commits
            .lock()
            .unwrap()
            .push(meta.meta.commit_sha.clone());
        Ok(())
    }
    async fn commit_exists(&self, sha: &str) -> Result<bool> {
        // If we have a watermark matching sha, consider it already indexed.
        let wm = self.watermark.lock().unwrap().clone();
        Ok(wm.as_deref() == Some(sha)
            || self.seen_commits.lock().unwrap().contains(&sha.to_string()))
    }
    async fn put_hunks(&self, _: &RepoId, rows: &[StorageHunkRecord]) -> Result<()> {
        self.put_hunk_totals.lock().unwrap().push(rows.len());
        let mut paths = self.put_hunk_paths.lock().unwrap();
        for row in rows {
            paths.push(row.hunk.file_path.clone());
        }
        Ok(())
    }
    async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> {
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
    ) -> Result<Vec<HunkHit>> {
        Ok(vec![])
    }
    async fn bm25_hunks_by_text(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<Vec<HunkHit>> {
        Ok(vec![])
    }
    async fn bm25_hunks_by_semantic_text(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<Vec<HunkHit>> {
        Ok(vec![])
    }
    async fn bm25_hunks_by_symbol_name(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<Vec<HunkHit>> {
        Ok(vec![])
    }
    async fn bm25_hunks_by_historical_symbol(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<Vec<HunkHit>> {
        Ok(vec![])
    }
    async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> Result<Vec<HunkSymbol>> {
        Ok(vec![])
    }
    async fn get_hunk_symbols_batch(
        &self,
        _: &RepoId,
        _: &[HunkId],
    ) -> Result<std::collections::HashMap<HunkId, Vec<HunkSymbol>>> {
        Ok(std::collections::HashMap::new())
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
    async fn get_index_metadata(&self, _: &RepoId) -> Result<StoredIndexMetadata> {
        Ok(StoredIndexMetadata::default())
    }
    async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> {
        Ok(())
    }
}

/// A CommitSource that returns an error from `hunks_for_commit` for a
/// specific SHA ("poison"). All other SHAs return one trivial hunk.
/// Used by the failure-isolation regression test (plan-28 task C.3).
struct PoisonedCommitSource {
    commits: Vec<CommitMeta>,
    poison_sha: String,
}

#[async_trait]
impl crate::indexer::CommitSource for PoisonedCommitSource {
    async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
        Ok(self.commits.clone())
    }
    async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>> {
        if sha == self.poison_sha {
            return Err(crate::OhraError::Git(format!(
                "poisoned hunks_for_commit: {sha}"
            )));
        }
        Ok(vec![hunk(sha)])
    }
}

fn hunk(sha: &str) -> Hunk {
    use crate::types::ChangeKind;
    Hunk {
        commit_sha: sha.into(),
        file_path: "src/lib.rs".into(),
        language: None,
        change_kind: ChangeKind::Added,
        diff_text: "+fn x() {}\n".into(),
    }
}

#[tokio::test]
async fn coordinator_indexes_single_commit_end_to_end() {
    let storage = Arc::new(SpyStorage::default());
    let coordinator = Coordinator::new(storage.clone(), Arc::new(ZeroEmbedder { dim: 4 }));
    let repo = RepoId::from_parts("sha", "/repo");
    let source = SingleCommitSource {
        sha: "abc".into(),
        hunks: vec![hunk("abc")],
    };
    coordinator
        .run(&repo, Arc::new(source), Arc::new(NoopSymbolSource))
        .await
        .unwrap();

    assert_eq!(
        *storage.put_commit_calls.lock().unwrap(),
        vec!["abc"],
        "coordinator must persist exactly one commit"
    );
    assert_eq!(
        *storage.put_hunk_totals.lock().unwrap(),
        vec![1],
        "coordinator must persist one hunk"
    );
}

#[tokio::test]
async fn coordinator_resumes_skipping_already_indexed_commit() {
    // Watermark is already at "abc" — the commit source still
    // returns "abc" but the coordinator must skip it.
    let storage = Arc::new(SpyStorage {
        watermark: Mutex::new(Some("abc".into())),
        ..Default::default()
    });
    let coordinator = Coordinator::new(storage.clone(), Arc::new(ZeroEmbedder { dim: 4 }));
    let repo = RepoId::from_parts("sha", "/repo");
    let source = SingleCommitSource {
        sha: "abc".into(),
        hunks: vec![hunk("abc")],
    };
    coordinator
        .run(&repo, Arc::new(source), Arc::new(NoopSymbolSource))
        .await
        .unwrap();

    assert!(
        storage.put_commit_calls.lock().unwrap().is_empty(),
        "coordinator must not re-index an already-indexed commit"
    );
}

#[tokio::test]
async fn coordinator_resume_from_attributed_hunks_directly() {
    use crate::indexer::stages::attribute::AttributedHunk;
    use crate::indexer::stages::hunk_chunk::HunkRecord;

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });
    let coordinator = Coordinator::new(storage.clone(), embedder);

    let repo = RepoId::from_parts("sha", "/repo");
    let commit = CommitMeta {
        commit_sha: "abc".into(),
        parent_sha: None,
        is_merge: false,
        author: None,
        ts: 1_000_000,
        message: "add feature".into(),
    };
    let attributed = vec![AttributedHunk {
        record: HunkRecord {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn x() {}\n".into(),
            semantic_text: "fn x() {}".into(),
            source_hunk: Hunk::default(),
        },
        symbols: None,
        attributed_semantic_text: None,
    }];

    coordinator
        .run_from_attributed(&repo, &commit, attributed)
        .await
        .unwrap();

    assert_eq!(
        *storage.put_commit_calls.lock().unwrap(),
        vec!["abc"],
        "partial-pipeline run must still persist the commit"
    );
    assert_eq!(
        *storage.put_hunk_totals.lock().unwrap(),
        vec![1],
        "partial-pipeline run must persist the hunk"
    );
}

#[tokio::test]
async fn coordinator_with_ignore_filter_field_is_set() {
    // Plan 26: builder smoke test — `with_ignore_filter` accepts an
    // `Arc<dyn IgnoreFilter>` and returns `Self`. Behavioural coverage
    // (mixed-path filtering, 100%-ignored watermark advance) lives in
    // `ignored_paths_drop_from_hunk_records_before_persist` and
    // `fully_ignored_commit_advances_watermark_with_zero_persisted_rows`.
    use crate::ignore::LayeredIgnore;

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });

    let filter: Arc<dyn crate::IgnoreFilter> = Arc::new(LayeredIgnore::builtins_only());
    let coord = Coordinator::new(storage, embedder).with_ignore_filter(filter);
    let _ = coord;
}

#[tokio::test]
async fn ignored_paths_drop_from_hunk_records_before_persist() {
    // Plan 26 Task C.2: when the filter ignores `vendor/foo.c`, the
    // pipeline must NOT persist a hunk for that path. The same
    // commit's `src/main.rs` hunk must still be persisted.
    use crate::ignore::LayeredIgnore;
    use crate::types::ChangeKind;

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });

    // Two hunks in one commit: one real, one vendor.
    let source = SingleCommitSource {
        sha: "abc".into(),
        hunks: vec![
            Hunk {
                commit_sha: "abc".into(),
                file_path: "src/main.rs".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: "+fn main() {}\n".into(),
            },
            Hunk {
                commit_sha: "abc".into(),
                file_path: "vendor/foo.c".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: "+int main(void) { return 0; }\n".into(),
            },
        ],
    };

    // Filter that ignores `vendor/` paths.
    let filter: Arc<dyn crate::IgnoreFilter> =
        Arc::new(LayeredIgnore::from_strings(&[], "", "vendor/\n"));
    let coord = Coordinator::new(storage.clone(), embedder).with_ignore_filter(filter);

    let repo = RepoId::from_parts("sha", "/repo");
    coord
        .run(&repo, Arc::new(source), Arc::new(NoopSymbolSource))
        .await
        .unwrap();

    // Commit must still be persisted (non-ignored hunk survived).
    assert_eq!(
        *storage.put_commit_calls.lock().unwrap(),
        vec!["abc"],
        "commit must be persisted when at least one hunk survives the filter"
    );

    // Exactly one hunk must reach storage, and it must be src/main.rs.
    let paths = storage.put_hunk_paths.lock().unwrap().clone();
    assert_eq!(
        paths.len(),
        1,
        "only one hunk must survive the ignore filter"
    );
    assert_eq!(
        paths[0], "src/main.rs",
        "the surviving hunk must be src/main.rs, not vendor/foo.c"
    );
}

#[tokio::test]
async fn fully_ignored_commit_advances_watermark_with_zero_persisted_rows() {
    // Plan 26 Task C.3: a commit whose changed paths are 100%
    // ignored must (a) persist zero hunks, (b) write no commit
    // metadata row, (c) still advance the coordinator's
    // `latest_sha` so `commits_behind_head` decreases on next run.
    use crate::ignore::LayeredIgnore;
    use crate::types::ChangeKind;

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });

    // One commit whose only changed paths are under vendor/ — 100% ignored.
    let source = SingleCommitSource {
        sha: "deadbeef".into(),
        hunks: vec![
            Hunk {
                commit_sha: "deadbeef".into(),
                file_path: "vendor/a.c".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: "+int a(void) { return 1; }\n".into(),
            },
            Hunk {
                commit_sha: "deadbeef".into(),
                file_path: "vendor/b.c".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: "+int b(void) { return 2; }\n".into(),
            },
        ],
    };

    // Filter that ignores all `vendor/` paths.
    let filter: Arc<dyn crate::IgnoreFilter> =
        Arc::new(LayeredIgnore::from_strings(&[], "", "vendor/\n"));
    let coord = Coordinator::new(storage.clone(), embedder).with_ignore_filter(filter);

    let repo = RepoId::from_parts("sha", "/repo");
    let result = coord
        .run_timed(&repo, Arc::new(source), Arc::new(NoopSymbolSource))
        .await
        .unwrap();

    assert_eq!(
        result.new_commits, 0,
        "no new commit rows for a 100%-ignored commit"
    );
    assert_eq!(result.new_hunks, 0, "no hunks for a 100%-ignored commit");
    assert_eq!(
        result.latest_sha.as_deref(),
        Some("deadbeef"),
        "watermark must still advance to the skipped commit's SHA"
    );

    // SpyStorage must have seen zero persisted commit rows.
    let commit_calls = storage.put_commit_calls.lock().unwrap().clone();
    assert!(
        commit_calls.is_empty(),
        "put_commit must not be called for a 100%-ignored commit; got {commit_calls:?}"
    );

    // SpyStorage must have seen zero persisted hunk paths.
    let persisted = storage.put_hunk_paths.lock().unwrap().clone();
    assert!(
        persisted.is_empty(),
        "expected zero persisted hunks; got {persisted:?}"
    );
}

#[tokio::test]
async fn worker_error_on_one_commit_does_not_block_others() {
    // Plan 28 Task C.3: a CommitSource that fails for one specific SHA.
    // Expect 9 of 10 commits to persist; the failed one is skipped and
    // the run completes successfully with result.new_commits == 9.
    let poison_sha = "poison-sha".to_string();

    // Build 10 commits: 9 normal + 1 poison.
    let mut commits = Vec::with_capacity(10);
    for i in 0..9u32 {
        commits.push(CommitMeta {
            commit_sha: format!("commit-{i:02}"),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_000_000 + i as i64,
            message: format!("commit {i}"),
        });
    }
    commits.push(CommitMeta {
        commit_sha: poison_sha.clone(),
        parent_sha: None,
        is_merge: false,
        author: None,
        ts: 1_000_009,
        message: "poisoned commit".into(),
    });

    let storage = Arc::new(SpyStorage::default());
    let embedder = Arc::new(ZeroEmbedder { dim: 4 });
    let source = Arc::new(PoisonedCommitSource {
        commits,
        poison_sha: poison_sha.clone(),
    });

    let coord = Coordinator::new(storage.clone(), embedder).with_workers(4);
    let repo = RepoId::from_parts("sha", "/repo");

    let result = coord
        .run_timed(&repo, source, Arc::new(NoopSymbolSource))
        .await
        .expect("run must succeed even when one commit errors");

    assert_eq!(
        result.new_commits, 9,
        "9 of 10 commits must be persisted; got {}",
        result.new_commits
    );
    assert_eq!(
        result.commits_failed, 1,
        "exactly 1 commit must be recorded as failed"
    );

    // Verify the poisoned SHA was NOT persisted.
    let persisted = storage.put_commit_calls.lock().unwrap().clone();
    assert!(
        !persisted.iter().any(|sha| sha == &poison_sha),
        "poison-sha must not have been persisted; got {persisted:?}"
    );
    // All 9 normal commits must appear.
    for i in 0..9u32 {
        let expected = format!("commit-{i:02}");
        assert!(
            persisted.iter().any(|sha| sha == &expected),
            "{expected} must have been persisted"
        );
    }
}
