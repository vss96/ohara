//! Tests for `RetrievalEngine`. Moved here from `engine.rs` to keep
//! `engine.rs` under the 500-line limit (plan-21 cleanup).

use super::*;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Serialises tests that mutate `OHARA_HOME` (a process-global env var).
/// Mirrors the pattern used in `ohara-core/src/paths.rs` and `ohara-cli` tests.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct DummyEmbedder;

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for DummyEmbedder {
    fn dimension(&self) -> usize {
        384
    }

    fn model_id(&self) -> &str {
        "dummy"
    }

    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}

pub(crate) struct DummyReranker;

#[async_trait::async_trait]
impl ohara_core::embed::RerankProvider for DummyReranker {
    async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
        Ok(vec![0.0; candidates.len()])
    }
}

pub(crate) fn make_test_engine() -> RetrievalEngine {
    RetrievalEngine::new(Arc::new(DummyEmbedder), Arc::new(DummyReranker))
}

pub(crate) fn build_test_repo(dir: &Path) {
    use git2::{Repository, Signature};
    let repo = Repository::init(dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn one() {}\n").unwrap();
    let sig = Signature::now("a", "a@a").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("a.rs")).unwrap();
    idx.write().unwrap();
    let tree_id = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

// env_lock is held across awaits intentionally: OHARA_HOME must remain
// stable from indexing through open_repo so both resolve the same DB path.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn explain_change_returns_one_blame_range_for_single_commit_repo() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());

    // Index the repo so storage has the commit metadata the explain
    // orchestrator needs to hydrate blame ranges into ExplainHits.
    // Canonicalize matches the path that open_repo will derive below.
    let canonical = std::fs::canonicalize(tmp.path()).unwrap();
    {
        let walker = ohara_git::GitWalker::open(&canonical).unwrap();
        let first = walker.first_commit_sha().unwrap();
        let repo_id = ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
        let storage: Arc<dyn ohara_core::Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
        let commit_src = Arc::new(ohara_git::GitCommitSource::open(&canonical).unwrap());
        let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&canonical).unwrap());
        let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
        indexer.run(&repo_id, commit_src, symbol_src).await.unwrap();
    }

    let engine = make_test_engine();
    let q = ohara_core::explain::ExplainQuery {
        file: "a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
        include_related: false,
    };
    let out = engine.explain_change(&canonical, q).await.expect("explain");
    // Single-commit repo: line 1 of a.rs blames to the only commit.
    assert_eq!(out.hits.len(), 1, "expected exactly one blame hit");
}

#[tokio::test]
async fn embed_query_uses_cache_on_repeat_call() {
    use std::sync::Mutex;
    struct Counting {
        calls: Mutex<usize>,
    }
    #[async_trait::async_trait]
    impl ohara_core::EmbeddingProvider for Counting {
        fn dimension(&self) -> usize {
            384
        }
        fn model_id(&self) -> &str {
            "counting"
        }
        async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
            *self.calls.lock().expect("not poisoned") += 1;
            Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
        }
    }
    let counting = std::sync::Arc::new(Counting {
        calls: Mutex::new(0),
    });
    let engine = RetrievalEngine::new(counting.clone(), std::sync::Arc::new(DummyReranker));
    let _ = engine.embed_query("hello").await.expect("first");
    let _ = engine.embed_query("hello").await.expect("second");
    assert_eq!(
        *counting.calls.lock().expect("not poisoned"),
        1,
        "second call must hit cache"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_meta_cached_within_ttl() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let engine = make_test_engine();
    let q = ohara_core::query::PatternQuery {
        query: "hello".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let _r1 = engine
        .find_pattern(tmp.path(), q.clone())
        .await
        .expect("first call");
    let hits_before = engine.meta_hits();
    let _r2 = engine
        .find_pattern(tmp.path(), q)
        .await
        .expect("second call");
    let hits_after = engine.meta_hits();
    assert_eq!(
        hits_after - hits_before,
        1,
        "second call must hit MetaCache"
    );
}

// Hold `env_lock` for the whole test: OHARA_HOME is read lazily inside
// `find_pattern`, and another test may repoint it at a TempDir it is about
// to delete (seen as a SQLite "disk I/O error" on open).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_returns_empty_hits_on_empty_index() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let engine = make_test_engine();
    let q = ohara_core::query::PatternQuery {
        query: "hello".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let out = engine
        .find_pattern(tmp.path(), q)
        .await
        .expect("find_pattern");
    assert!(
        out.hits.is_empty(),
        "empty index → empty hits, got {:?}",
        out.hits
    );
}

// `env_lock` serialises tests that mutate OHARA_HOME (process-global). It is
// held for the whole test because `open_repo` reads OHARA_HOME lazily.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn open_repo_caches_handle_by_repo_id() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());

    let engine = make_test_engine();
    let h1 = engine.open_repo(tmp.path()).await.expect("first open");
    let h2 = engine.open_repo(tmp.path()).await.expect("second open");
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "open_repo must return the cached Arc, not a fresh handle"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn invalidate_repo_drops_handle_and_meta() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let tmp = tempfile::tempdir().unwrap();
    build_test_repo(tmp.path());
    let engine = make_test_engine();
    let h1 = engine.open_repo(tmp.path()).await.expect("first open");
    engine
        .invalidate_repo(tmp.path())
        .await
        .expect("invalidate");
    let h2 = engine.open_repo(tmp.path()).await.expect("second open");
    assert!(
        !Arc::ptr_eq(&h1, &h2),
        "invalidated handle must be re-opened, got the same Arc"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn explain_change_blame_cache_hit_on_second_call() {
    // Plan 21 Task E.1: calling explain_change twice for the same
    // file on an unchanged HEAD must result in a BlameCache hit on
    // the second call — i.e., Blamer::blame_range is NOT called a
    // second time. We verify this indirectly by asserting that the
    // second call returns an identical result without error, and that
    // blame_cache_hits() increments.
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());

    // Index so storage has the commit metadata.
    let canonical = std::fs::canonicalize(tmp.path()).unwrap();
    {
        let walker = ohara_git::GitWalker::open(&canonical).unwrap();
        let first = walker.first_commit_sha().unwrap();
        let repo_id = ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
        let storage: Arc<dyn ohara_core::Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
        let commit_src = Arc::new(ohara_git::GitCommitSource::open(&canonical).unwrap());
        let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&canonical).unwrap());
        let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
        indexer.run(&repo_id, commit_src, symbol_src).await.unwrap();
    }

    let engine = make_test_engine();
    let q = ohara_core::explain::ExplainQuery {
        file: "a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
        include_related: false,
    };

    // First call: cache miss → Blamer runs → result cached.
    let r1 = engine
        .explain_change(&canonical, q.clone())
        .await
        .expect("first explain");

    // Second call: cache hit → Blamer skipped → same result.
    let r2 = engine
        .explain_change(&canonical, q)
        .await
        .expect("second explain");

    // Both calls must produce the same number of hits (single-commit repo).
    assert_eq!(
        r1.hits.len(),
        r2.hits.len(),
        "second call must return same result as first"
    );
    assert_eq!(
        r1.hits.len(),
        1,
        "single-commit repo must produce exactly one blame hit"
    );

    assert_eq!(
        engine.blame_cache_hits(),
        1,
        "second call must increment blame_cache_hits by 1"
    );
}

// env_lock held across awaits intentionally — see note above the
// explain_change test.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_refuses_when_index_needs_rebuild() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let canonical = std::fs::canonicalize(tmp.path()).unwrap();

    // Index, then sabotage the vector-affecting metadata so
    // CompatibilityStatus::assess yields NeedsRebuild.
    {
        let walker = ohara_git::GitWalker::open(&canonical).unwrap();
        let first = walker.first_commit_sha().unwrap();
        let repo_id = ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
        let storage: Arc<dyn ohara_core::Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
        let commit_src = Arc::new(ohara_git::GitCommitSource::open(&canonical).unwrap());
        let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&canonical).unwrap());
        let indexer = ohara_core::Indexer::new(storage.clone(), Arc::new(DummyEmbedder));
        indexer.run(&repo_id, commit_src, symbol_src).await.unwrap();
        storage
            .put_index_metadata(
                &repo_id,
                &[("embedding_model".to_string(), "other-model".to_string())],
            )
            .await
            .unwrap();
    }

    let engine = make_test_engine();
    let q = ohara_core::query::PatternQuery {
        query: "one".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let err = engine
        .find_pattern(&canonical, q)
        .await
        .expect_err("incompatible index must refuse");
    assert!(
        matches!(err, EngineError::NeedsRebuild { .. }),
        "expected NeedsRebuild, got: {err:?}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn index_status_returns_meta_for_indexed_repo() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let engine = make_test_engine();
    let meta = engine.index_status(tmp.path()).await.expect("index_status");
    // Empty stored metadata → compatibility is present but not NeedsRebuild.
    let is_rebuild = matches!(
        meta.compatibility,
        Some(ohara_core::index_metadata::CompatibilityStatus::NeedsRebuild { .. })
    );
    assert!(!is_rebuild, "fresh test repo must not need rebuild");
}

#[tokio::test]
async fn unload_idle_reranker_with_dummy_is_false() {
    let engine = make_test_engine();
    // DummyReranker uses the trait default — nothing to unload.
    assert!(!engine.unload_idle_reranker(std::time::Duration::ZERO).await);
}

// ---- plan-33: one store per repo, one handle per worktree ----

/// Commit `body` as `file` on the current HEAD of `repo`.
fn commit_file(repo: &git2::Repository, file: &str, body: &str, msg: &str) -> git2::Oid {
    let wd = repo.workdir().unwrap().to_path_buf();
    std::fs::write(wd.join(file), body).unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(file)).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("a", "a@a").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
        .unwrap()
}

/// Index the checkout at `path` into the shared store, recording its tip
/// under its own worktree key (main → "", linked → its name).
async fn index_checkout(path: &Path) {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
    ohara_core::StorageWrite::open_repo(
        sqlite.as_ref(),
        &id.repo_id,
        &id.id_path,
        &id.first_commit_sha,
    )
    .await
    .unwrap();
    let head = git2::Repository::open(&id.worktree_path)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    let commit_src = Arc::new(ohara_git::GitCommitSource::open(&id.worktree_path).unwrap());
    let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&id.worktree_path).unwrap());
    let indexer = ohara_core::Indexer::new(sqlite.clone(), Arc::new(DummyEmbedder))
        .with_runtime_metadata(crate::current_runtime_metadata(
            ohara_core::EmbedMode::Semantic,
        ))
        .with_tip_context(ohara_core::TipContext {
            tips: sqlite.clone(),
            visibility: Arc::new(ohara_git::GitVisibility::open(&id.worktree_path).unwrap()),
            worktree: id.worktree.clone(),
            head: ohara_core::CommitSha::parse(&head).unwrap(),
        });
    indexer
        .run(&id.repo_id, commit_src, symbol_src)
        .await
        .unwrap();
}

/// Main clone with two commits, indexed, plus a linked worktree `slot-1`
/// on a new branch at main's HEAD. Caller must have set `OHARA_HOME`.
async fn repo_with_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    let repo = git2::Repository::open(main.path()).unwrap();
    commit_file(&repo, "b.rs", "fn two() {}\n", "second");
    index_checkout(main.path()).await;
    let parent = main.path().join(".wt");
    std::fs::create_dir_all(&parent).unwrap();
    let wt = parent.join("slot-1");
    repo.worktree("slot-1", &wt, None).unwrap();
    (main, std::fs::canonicalize(wt).unwrap())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worktree_and_main_share_one_store_but_not_one_handle() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, wt) = repo_with_worktree().await;
    let engine = make_test_engine();
    let m = engine.open_repo(main.path()).await.expect("open main");
    let w = engine.open_repo(&wt).await.expect("open worktree");
    assert!(Arc::ptr_eq(&m.store, &w.store), "store must be shared");
    assert_eq!(m.repo_id, w.repo_id);
    assert_ne!(m.repo_path, w.repo_path);
    assert_eq!(w.worktree.as_str(), "slot-1");
    assert!(m.worktree.is_main());
    assert_ne!(m.blamer.repo_path(), w.blamer.repo_path());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worktree_needs_no_reindex_to_report_fresh() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, wt) = repo_with_worktree().await;
    let engine = make_test_engine();
    let meta = engine.index_status(&wt).await.expect("index_status");
    assert_eq!(meta.index_status.commits_behind_head, 0);
    assert!(meta.index_status.last_indexed_commit.is_some());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn commits_behind_is_per_worktree() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, wt) = repo_with_worktree().await;
    let wt_repo = git2::Repository::open(&wt).unwrap();
    commit_file(&wt_repo, "c.rs", "fn three() {}\n", "worktree only");
    let engine = make_test_engine();
    let w = engine.index_status(&wt).await.expect("worktree status");
    let m = engine.index_status(main.path()).await.expect("main status");
    assert_eq!(w.index_status.commits_behind_head, 1);
    assert_eq!(m.index_status.commits_behind_head, 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn invalidate_repo_drops_every_worktree_handle() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, wt) = repo_with_worktree().await;
    let engine = make_test_engine();
    let m1 = engine.open_repo(main.path()).await.expect("open main");
    let w1 = engine.open_repo(&wt).await.expect("open worktree");
    engine.invalidate_repo(&wt).await.expect("invalidate");
    let m2 = engine.open_repo(main.path()).await.expect("reopen main");
    assert!(!Arc::ptr_eq(&m1.store, &m2.store), "store must be reopened");
    assert!(!Arc::ptr_eq(&m1, &m2), "main handle must be dropped");
    let w2 = engine.open_repo(&wt).await.expect("reopen worktree");
    assert!(!Arc::ptr_eq(&w1, &w2), "worktree handle must be dropped");
    assert!(Arc::ptr_eq(&m2.store, &w2.store));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn blame_cache_does_not_cross_worktrees() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, wt) = repo_with_worktree().await;
    // Rewrite a.rs line 1 in the worktree, then restore it: same blob at
    // both HEADs, but the worktree's line 1 is last touched by a
    // worktree-only commit.
    let wt_repo = git2::Repository::open(&wt).unwrap();
    commit_file(&wt_repo, "a.rs", "fn changed() {}\n", "rewrite");
    commit_file(&wt_repo, "a.rs", "fn one() {}\n", "restore");
    index_checkout(&wt).await;
    let engine = make_test_engine();
    let q = ohara_core::explain::ExplainQuery {
        file: "a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
        include_related: false,
    };
    let m = engine
        .explain_change(main.path(), q.clone())
        .await
        .expect("explain main");
    let w = engine
        .explain_change(&wt, q)
        .await
        .expect("explain worktree");
    assert_eq!(m.hits.len(), 1);
    assert_eq!(w.hits.len(), 1);
    assert_ne!(
        m.hits[0].commit_sha, w.hits[0].commit_sha,
        "worktree blame must not be served from main's cache entry"
    );
}

// ---- plan-33: per-worktree visibility ----

fn frobnicate_query() -> ohara_core::query::PatternQuery {
    ohara_core::query::PatternQuery {
        query: "frobnicate retry".into(),
        k: 10,
        language: None,
        since_unix: None,
        no_rerank: true,
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn run_branch_commit_is_visible_only_from_its_own_worktree() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let repo = git2::Repository::open(main.path()).unwrap();
    let slot2 = main.path().join(".wt").join("slot-2");
    repo.worktree("slot-2", &slot2, None).unwrap();
    let slot2 = std::fs::canonicalize(slot2).unwrap();

    let wt_repo = git2::Repository::open(&slot1).unwrap();
    let w = commit_file(
        &wt_repo,
        "retry.rs",
        "fn frobnicate_retry() { /* frobnicate retry with backoff */ }\n",
        "add frobnicate retry",
    )
    .to_string();
    index_checkout(&slot1).await;

    let engine = make_test_engine();
    let from_slot1 = engine
        .find_pattern(&slot1, frobnicate_query())
        .await
        .expect("slot-1");
    assert!(
        from_slot1.hits.iter().any(|h| h.commit_sha == w),
        "slot-1 must see W, got {:?}",
        from_slot1
            .hits
            .iter()
            .map(|h| &h.commit_sha)
            .collect::<Vec<_>>()
    );
    for (name, path) in [("main", main.path()), ("slot-2", slot2.as_path())] {
        let out = engine
            .find_pattern(path, frobnicate_query())
            .await
            .expect(name);
        assert!(
            out.hits.iter().all(|h| h.commit_sha != w),
            "{name} must not see W"
        );
    }
    assert!(engine.ancestry_filtered_queries() >= 2);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn filter_is_skipped_when_every_tip_is_visible() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, _slot1) = repo_with_worktree().await;
    let engine = make_test_engine();
    engine
        .find_pattern(main.path(), frobnicate_query())
        .await
        .expect("find_pattern");
    assert_eq!(engine.ancestry_filtered_queries(), 0);
}

// ---- plan-33: daemon-side worktree indexing and prune ----

/// Embedder that records every text it is asked to embed. It reports
/// `model` as its id; the fixtures index with the default model's
/// metadata, so `ohara_embed::DEFAULT_MODEL_ID` is the consistent choice.
struct RecordingEmbedder {
    model: &'static str,
    texts: Mutex<Vec<String>>,
}

impl RecordingEmbedder {
    fn with_model(model: &'static str) -> Self {
        Self {
            model,
            texts: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.texts.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for RecordingEmbedder {
    fn dimension(&self) -> usize {
        384
    }

    fn model_id(&self) -> &str {
        self.model
    }

    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        self.texts.lock().unwrap().extend(texts.iter().cloned());
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}

fn recording_engine() -> (Arc<RecordingEmbedder>, RetrievalEngine) {
    recording_engine_with_model(ohara_embed::DEFAULT_MODEL_ID)
}

fn recording_engine_with_model(model: &'static str) -> (Arc<RecordingEmbedder>, RetrievalEngine) {
    let rec = Arc::new(RecordingEmbedder::with_model(model));
    let engine = RetrievalEngine::new(rec.clone(), Arc::new(DummyReranker));
    (rec, engine)
}

fn sha_of(oid: git2::Oid) -> ohara_core::CommitSha {
    ohara_core::CommitSha::parse(&oid.to_string()).unwrap()
}

/// Every commit row in the shared store of the repo at `path`.
async fn stored_commits(path: &Path) -> Vec<String> {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    ohara_core::TipStore::indexed_commit_shas(&sqlite, &id.repo_id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ensure_indexed_is_a_noop_when_the_sha_is_covered() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, _slot1) = repo_with_worktree().await;
    let repo = git2::Repository::open(main.path()).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let parent = head.parent_id(0).unwrap();
    let (rec, engine) = recording_engine();
    let out = engine
        .ensure_indexed(main.path(), &sha_of(parent))
        .await
        .expect("ensure_indexed");
    assert!(!out.indexed);
    assert_eq!(out.new_commits, 0);
    assert_eq!(rec.calls(), 0, "a covered sha must not embed anything");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ensure_indexed_runs_one_incremental_index_for_an_uncovered_head() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, slot1) = repo_with_worktree().await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    let new = commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third");
    let (_rec, engine) = recording_engine();
    let first = engine
        .ensure_indexed(&slot1, &sha_of(new))
        .await
        .expect("first ensure_indexed");
    assert!(first.indexed);
    assert_eq!(first.new_commits, 1);
    let second = engine
        .ensure_indexed(&slot1, &sha_of(new))
        .await
        .expect("second ensure_indexed");
    assert!(!second.indexed);
    assert_eq!(second.new_commits, 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn indexing_a_worktree_embeds_only_its_new_commits() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let before = stored_commits(main.path()).await.len();
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    commit_file(&wt_repo, "alpha.rs", "fn alpha_marker() {}\n", "add alpha");
    commit_file(&wt_repo, "beta.rs", "fn beta_marker() {}\n", "add beta");
    let (rec, engine) = recording_engine();
    let out = engine.index_worktree(&slot1).await.expect("index_worktree");
    assert!(out.indexed);
    assert_eq!(out.new_commits, 2);
    assert_eq!(stored_commits(main.path()).await.len(), before + 2);
    let texts = rec.texts.lock().unwrap().clone();
    assert!(!texts.is_empty(), "the new commits must be embedded");
    for t in &texts {
        assert!(
            t.contains("alpha") || t.contains("beta"),
            "embedded text from an already-indexed commit: {t:?}"
        );
    }
    assert!(texts.iter().any(|t| t.contains("alpha_marker")));
    assert!(texts.iter().any(|t| t.contains("beta_marker")));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ensure_indexed_refuses_an_unindexed_repo() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let tmp = tempfile::tempdir().unwrap();
    build_test_repo(tmp.path());
    let repo = git2::Repository::open(tmp.path()).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap().id();
    let (rec, engine) = recording_engine();
    let err = engine
        .ensure_indexed(tmp.path(), &sha_of(head))
        .await
        .expect_err("never-indexed repo must refuse");
    assert!(
        matches!(err, EngineError::NoIndex { .. }),
        "expected NoIndex, got: {err:?}"
    );
    assert_eq!(rec.calls(), 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn daemon_indexing_refuses_an_incompatible_index() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    {
        let id = ohara_git::RepoIdentity::resolve(main.path()).unwrap();
        let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
        let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
        ohara_core::StorageStatus::put_index_metadata(
            &sqlite,
            &id.repo_id,
            &[("embedding_model".to_string(), "other-model".to_string())],
        )
        .await
        .unwrap();
    }
    let before = stored_commits(main.path()).await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    let new = commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third");
    let (rec, engine) = recording_engine();
    let err = engine
        .ensure_indexed(&slot1, &sha_of(new))
        .await
        .expect_err("incompatible index must refuse");
    assert!(
        matches!(err, EngineError::NeedsRebuild { .. }),
        "expected NeedsRebuild, got: {err:?}"
    );
    let err = engine
        .index_worktree(&slot1)
        .await
        .expect_err("index_worktree must refuse too");
    assert!(matches!(err, EngineError::NeedsRebuild { .. }));
    assert_eq!(stored_commits(main.path()).await, before);
    assert_eq!(rec.calls(), 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn prune_deletes_commits_of_a_deleted_branch_and_its_tip() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let main_commits = stored_commits(main.path()).await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    let extra = commit_file(&wt_repo, "c.rs", "fn three() {}\n", "slot only");
    drop(wt_repo);
    index_checkout(&slot1).await;
    assert!(stored_commits(main.path())
        .await
        .contains(&extra.to_string()));

    // Remove the worktree, then delete its branch.
    std::fs::remove_dir_all(&slot1).unwrap();
    let repo = git2::Repository::open(main.path()).unwrap();
    repo.find_worktree("slot-1")
        .unwrap()
        .prune(Some(git2::WorktreePruneOptions::new().working_tree(true)))
        .unwrap();
    repo.find_branch("slot-1", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    let engine = make_test_engine();
    let out = engine
        .prune_unreachable(main.path())
        .await
        .expect("prune_unreachable");
    assert_eq!(out.commits_deleted, 1);
    assert_eq!(out.tips_removed, 1);
    let left = stored_commits(main.path()).await;
    assert!(!left.contains(&extra.to_string()));
    for c in &main_commits {
        assert!(left.contains(c), "main commit {c} must survive prune");
    }
    let id = ohara_git::RepoIdentity::resolve(main.path()).unwrap();
    let handle = engine.open_repo(main.path()).await.unwrap();
    let tips = handle.store.tips.indexed_tips(&id.repo_id).await.unwrap();
    assert_eq!(tips.len(), 1);
    assert!(tips[0].worktree.is_main(), "the main tip is never removed");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn daemon_indexing_refuses_a_live_embedder_of_another_model() {
    // Review Focus 5: same dimension (384), different model id. Stored
    // metadata is the default model, so the binary-level check passes;
    // only the live-embedder guard can catch this.
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let before = stored_commits(main.path()).await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third");
    let (rec, engine) = recording_engine_with_model("other-384d-model");
    let err = engine
        .index_worktree(&slot1)
        .await
        .expect_err("a live embedder of another model must refuse");
    assert!(
        matches!(err, EngineError::NeedsRebuild { .. }),
        "expected NeedsRebuild, got: {err:?}"
    );
    assert_eq!(stored_commits(main.path()).await, before);
    assert_eq!(rec.calls(), 0);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn daemon_indexing_accepts_an_equivalent_model() {
    // The fp32 CoreML id is in the default model's equivalence class.
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, slot1) = repo_with_worktree().await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third");
    let (_rec, engine) = recording_engine_with_model("bge-small-en-v1.5");
    let out = engine
        .index_worktree(&slot1)
        .await
        .expect("equivalent model");
    assert_eq!(out.new_commits, 1);
}

// ---- plan-33 Task 12: background index queue ----

/// Embedder for queue tests. Batches that look like the `frobnicate`
/// query return at once; every other (index) batch first sleeps `delay`
/// and, when `gated`, waits for [`QueueEmbedder::release`].
struct QueueEmbedder {
    delay: std::time::Duration,
    gated: bool,
    started: tokio::sync::Notify,
    gate: tokio::sync::Semaphore,
    index_batches: std::sync::atomic::AtomicUsize,
}

impl QueueEmbedder {
    fn new(delay: std::time::Duration, gated: bool) -> Self {
        Self {
            delay,
            gated,
            started: tokio::sync::Notify::new(),
            gate: tokio::sync::Semaphore::new(0),
            index_batches: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn release(&self) {
        self.gate.add_permits(1 << 20);
    }
}

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for QueueEmbedder {
    fn dimension(&self) -> usize {
        384
    }

    fn model_id(&self) -> &str {
        ohara_embed::DEFAULT_MODEL_ID
    }

    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        let is_query = texts.len() == 1 && texts[0].contains("frobnicate");
        if !is_query {
            self.index_batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_one();
            tokio::time::sleep(self.delay).await;
            if self.gated {
                let _permit = self.gate.acquire().await.unwrap();
            }
        }
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}

/// The indexed tip recorded for the worktree at `path`, if any.
async fn tip_of(path: &Path) -> Option<String> {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    ohara_core::TipStore::indexed_tips(&sqlite, &id.repo_id)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.worktree == id.worktree)
        .map(|t| t.sha.as_str().to_string())
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn index_tip_returns_before_indexing_and_the_queue_catches_up() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, slot1) = repo_with_worktree().await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    let head = commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third").to_string();
    let emb = Arc::new(QueueEmbedder::new(
        std::time::Duration::from_millis(50),
        false,
    ));
    let engine = Arc::new(RetrievalEngine::new(emb.clone(), Arc::new(DummyReranker)));

    let t0 = std::time::Instant::now();
    assert!(engine.enqueue_index_tip(slot1.clone()));
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(20),
        "enqueue must not block on indexing: {:?}",
        t0.elapsed()
    );
    assert_eq!(
        emb.index_batches.load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    let stop = tokio_util::sync::CancellationToken::new();
    let worker = tokio::spawn(engine.clone().run_index_queue(stop.clone()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while tip_of(&slot1).await.as_deref() != Some(head.as_str()) {
        assert!(
            std::time::Instant::now() < deadline,
            "queue did not index slot-1 HEAD within 5 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    stop.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("worker must exit promptly after stop")
        .expect("worker join");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn repeated_index_tip_for_one_worktree_is_coalesced() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, slot1) = repo_with_worktree().await;
    let engine = make_test_engine();
    let queued: Vec<bool> = (0..3)
        .map(|_| engine.enqueue_index_tip(slot1.clone()))
        .collect();
    assert_eq!(queued, [true, false, false]);
    // A non-canonical spelling of the same worktree coalesces too.
    let dotted = slot1.join("..").join("slot-1");
    assert!(!engine.enqueue_index_tip(dotted));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_is_served_while_an_index_job_runs() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let wt_repo = git2::Repository::open(&slot1).unwrap();
    commit_file(&wt_repo, "c.rs", "fn three() {}\n", "third");
    let emb = Arc::new(QueueEmbedder::new(std::time::Duration::ZERO, true));
    let engine = Arc::new(RetrievalEngine::new(emb.clone(), Arc::new(DummyReranker)));

    let job = {
        let engine = engine.clone();
        let slot1 = slot1.clone();
        tokio::spawn(async move { engine.index_worktree(&slot1).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emb.started.notified())
        .await
        .expect("index job never reached the embedder");

    let served = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.find_pattern(main.path(), frobnicate_query()),
    )
    .await
    .expect("find_pattern must not wait for the index job");
    served.expect("find_pattern");
    assert!(!job.is_finished(), "the index job must still be running");

    emb.release();
    let out = job.await.unwrap().expect("index job");
    assert_eq!(out.new_commits, 1);
}

// ---- plan-33 final review I2: HEAD discovery ----

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_works_from_a_plain_clone_subdirectory() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    index_checkout(main.path()).await;
    let sub = main.path().join("src").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    let engine = make_test_engine();
    engine
        .find_pattern(&sub, frobnicate_query())
        .await
        .expect("find_pattern from a subdirectory of a plain clone");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_on_an_unborn_head_returns_empty_hits() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let engine = make_test_engine();
    let out = engine
        .find_pattern(repo.path(), frobnicate_query())
        .await
        .expect("unborn HEAD must not be an error");
    assert!(out.hits.is_empty(), "got {:?}", out.hits);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_from_a_linked_worktree_on_an_unborn_branch_is_not_an_error() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (_main, slot1) = repo_with_worktree().await;
    git2::Repository::open(&slot1)
        .unwrap()
        .set_head("refs/heads/orphan")
        .unwrap();
    let engine = make_test_engine();
    let out = engine
        .find_pattern(&slot1, frobnicate_query())
        .await
        .expect("unborn worktree HEAD must not be an error");
    assert!(out.hits.is_empty(), "got {:?}", out.hits);
}

// ---- plan-33 final review I1: retired tips ----

/// Point `repo`'s HEAD at a new branch `name` created at `at`, and check it out.
fn switch_to_new_branch(repo: &git2::Repository, name: &str, at: git2::Oid) {
    let commit = repo.find_commit(at).unwrap();
    repo.branch(name, &commit, true).unwrap();
    repo.set_head(&format!("refs/heads/{name}")).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn reused_slot_merged_into_main_does_not_leak_the_old_run() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let main_repo = git2::Repository::open(main.path()).unwrap();
    let main_head = main_repo.head().unwrap().peel_to_commit().unwrap().id();

    // Run A on slot-1: a1, a2, indexed.
    let wt = git2::Repository::open(&slot1).unwrap();
    let a1 = commit_file(
        &wt,
        "retry.rs",
        "fn frobnicate_retry() { /* frobnicate retry with backoff */ }\n",
        "run A: frobnicate retry",
    );
    let a2 = commit_file(
        &wt,
        "retry2.rs",
        "fn frobnicate_retry_again() { /* frobnicate retry jitter */ }\n",
        "run A: frobnicate retry jitter",
    );
    index_checkout(&slot1).await;

    // Slot reused for run B (from main), c1 indexed, then merged into main.
    switch_to_new_branch(&wt, "run-b", main_head);
    let c1 = commit_file(&wt, "c.rs", "fn three() {}\n", "run B");
    index_checkout(&slot1).await;
    let branch = main_repo.head().unwrap().name().unwrap().to_string();
    main_repo
        .reference(&branch, c1, true, "merge run B")
        .unwrap();
    main_repo
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    index_checkout(main.path()).await;

    let engine = make_test_engine();
    let out = engine
        .find_pattern(main.path(), frobnicate_query())
        .await
        .expect("find_pattern");
    let leaked: Vec<_> = out
        .hits
        .iter()
        .filter(|h| h.commit_sha == a1.to_string() || h.commit_sha == a2.to_string())
        .map(|h| h.commit_sha.clone())
        .collect();
    assert!(leaked.is_empty(), "run A leaked into main: {leaked:?}");
    assert!(engine.ancestry_filtered_queries() >= 1);
}

/// All tips of the shared store behind `path`, as `(key, sha)`.
async fn all_tips(path: &Path) -> Vec<(String, String)> {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    ohara_core::TipStore::indexed_tips(&sqlite, &id.repo_id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| (t.worktree.as_str().to_string(), t.sha.as_str().to_string()))
        .collect()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn prune_keeps_a_reachable_retired_tip_and_drops_it_once_its_branch_is_gone() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let main_repo = git2::Repository::open(main.path()).unwrap();
    let main_head = main_repo.head().unwrap().peel_to_commit().unwrap().id();
    let wt = git2::Repository::open(&slot1).unwrap();
    let a1 = commit_file(&wt, "c.rs", "fn three() {}\n", "run A").to_string();
    index_checkout(&slot1).await;
    switch_to_new_branch(&wt, "run-b", main_head);
    commit_file(&wt, "d.rs", "fn four() {}\n", "run B");
    index_checkout(&slot1).await;
    let retired = format!("slot-1/{a1}");
    assert!(all_tips(main.path())
        .await
        .contains(&(retired.clone(), a1.clone())));

    // Branch `slot-1` still holds run A: nothing is pruned.
    let engine = make_test_engine();
    let out = engine.prune_unreachable(main.path()).await.unwrap();
    assert_eq!((out.commits_deleted, out.tips_removed), (0, 0));
    assert!(all_tips(main.path())
        .await
        .contains(&(retired.clone(), a1.clone())));

    // Delete the branch: run A's commit and its retired tip go.
    main_repo
        .find_branch("slot-1", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();
    let out = engine.prune_unreachable(main.path()).await.unwrap();
    assert_eq!((out.commits_deleted, out.tips_removed), (1, 1));
    let tips = all_tips(main.path()).await;
    assert!(tips.iter().all(|(k, _)| k != &retired), "got {tips:?}");
    assert!(tips.iter().any(|(k, _)| k == "slot-1"), "live tip stays");
    assert!(!stored_commits(main.path()).await.contains(&a1));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn prune_retires_a_removed_worktrees_tip_while_its_branch_survives() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let wt = git2::Repository::open(&slot1).unwrap();
    let a1 = commit_file(&wt, "c.rs", "fn three() {}\n", "run A").to_string();
    drop(wt);
    index_checkout(&slot1).await;
    std::fs::remove_dir_all(&slot1).unwrap();
    let repo = git2::Repository::open(main.path()).unwrap();
    repo.find_worktree("slot-1")
        .unwrap()
        .prune(Some(git2::WorktreePruneOptions::new().working_tree(true)))
        .unwrap();

    let engine = make_test_engine();
    let out = engine.prune_unreachable(main.path()).await.unwrap();
    assert_eq!((out.commits_deleted, out.tips_removed), (0, 1));
    let tips = all_tips(main.path()).await;
    assert!(tips.iter().all(|(k, _)| k != "slot-1"), "got {tips:?}");
    assert!(
        tips.contains(&(format!("slot-1/{a1}"), a1.clone())),
        "branch slot-1 still reaches {a1}: its tip must stay as a retired tip, got {tips:?}"
    );
}

// ---- plan-33 final review I3: HEAD-symbol ownership ----

/// Names in the shared HEAD-symbol snapshot of the repo at `path`.
async fn head_symbol_names(path: &Path) -> Vec<String> {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    sqlite.head_symbol_names().await.unwrap()
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn main_indexes_after_a_linked_first_index_and_takes_over_head_symbols() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    let repo = git2::Repository::open(main.path()).unwrap();
    let main_head = commit_file(&repo, "b.rs", "fn two() {}\n", "second");
    let wt_path = main.path().join(".wt").join("slot-1");
    std::fs::create_dir_all(wt_path.parent().unwrap()).unwrap();
    repo.worktree("slot-1", &wt_path, None).unwrap();
    let slot1 = std::fs::canonicalize(wt_path).unwrap();
    let wt = git2::Repository::open(&slot1).unwrap();
    commit_file(&wt, "slot_only.rs", "fn slot_only() {}\n", "slot only");

    // Linked worktree indexes first: with no main tip it owns HEAD symbols.
    index_checkout(&slot1).await;
    assert!(tip_of(main.path()).await.is_none());
    assert!(head_symbol_names(main.path())
        .await
        .contains(&"slot_only".to_string()));

    // Main's HEAD is covered by slot-1's tip, yet main must still run.
    let (_rec, engine) = recording_engine();
    let out = engine
        .ensure_indexed(main.path(), &sha_of(main_head))
        .await
        .expect("ensure_indexed");
    assert!(out.indexed, "main without a tip must index");
    assert_eq!(tip_of(main.path()).await, Some(main_head.to_string()));
    let names = head_symbol_names(main.path()).await;
    assert!(names.contains(&"two".to_string()), "got {names:?}");
    assert!(!names.contains(&"slot_only".to_string()), "got {names:?}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn repeated_incremental_runs_replace_head_symbols_without_duplicates() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, _slot1) = repo_with_worktree().await;
    let repo = git2::Repository::open(main.path()).unwrap();
    commit_file(&repo, "c.rs", "fn three() {}\n", "third");
    index_checkout(main.path()).await;
    commit_file(&repo, "d.rs", "fn four() {}\n", "fourth");
    index_checkout(main.path()).await;
    let names = head_symbol_names(main.path()).await;
    let unique: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "duplicates: {names:?}");
    assert!(names.contains(&"four".to_string()), "got {names:?}");
}

// ---- plan-33 follow-up F4: retired tips are cleaned up automatically ----

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn daemon_index_after_an_amend_prunes_the_retired_tip_and_restores_the_fast_path() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    let repo = git2::Repository::open(main.path()).unwrap();
    let old = commit_file(&repo, "b.rs", "fn two() {}\n", "second");
    index_checkout(main.path()).await;

    // Amend HEAD: the indexed tip is no longer an ancestor of HEAD.
    let amended = repo
        .find_commit(old)
        .unwrap()
        .amend(
            Some("HEAD"),
            None,
            None,
            None,
            Some("second, amended"),
            None,
        )
        .unwrap();
    let (_rec, engine) = recording_engine();
    engine
        .index_worktree(main.path())
        .await
        .expect("index_worktree");

    let tips = all_tips(main.path()).await;
    assert_eq!(tips, vec![(String::new(), amended.to_string())]);
    assert!(!stored_commits(main.path()).await.contains(&old.to_string()));
    engine
        .find_pattern(main.path(), frobnicate_query())
        .await
        .expect("find_pattern");
    assert_eq!(engine.ancestry_filtered_queries(), 0, "fast path is back");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn auto_prune_skips_a_reachable_retired_tip_and_runs_once_its_branch_is_gone() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let main_repo = git2::Repository::open(main.path()).unwrap();
    let main_head = main_repo.head().unwrap().peel_to_commit().unwrap().id();
    let wt = git2::Repository::open(&slot1).unwrap();
    let a1 = commit_file(&wt, "c.rs", "fn three() {}\n", "run A").to_string();
    index_checkout(&slot1).await;

    // Branch switch: slot-1's tip a1 is retired, but branch `slot-1`
    // still reaches it, so prune could not drop it. No full prune runs.
    switch_to_new_branch(&wt, "run-b", main_head);
    commit_file(&wt, "d.rs", "fn four() {}\n", "run B");
    let (_rec, engine) = recording_engine();
    engine.index_worktree(&slot1).await.expect("index run B");
    commit_file(&wt, "e.rs", "fn five() {}\n", "run B again");
    engine
        .index_worktree(&slot1)
        .await
        .expect("index run B again");
    let retired = format!("slot-1/{a1}");
    assert!(all_tips(main.path())
        .await
        .iter()
        .any(|(k, _)| k == &retired));
    assert_eq!(engine.index_service.auto_prune_runs(), 0);

    // Delete the branch: the next index job notices and prunes.
    main_repo
        .find_branch("slot-1", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();
    commit_file(&wt, "f.rs", "fn six() {}\n", "run B third");
    engine
        .index_worktree(&slot1)
        .await
        .expect("index after delete");
    assert_eq!(engine.index_service.auto_prune_runs(), 1);
    let tips = all_tips(main.path()).await;
    assert!(tips.iter().all(|(k, _)| k != &retired), "got {tips:?}");
    assert!(!stored_commits(main.path()).await.contains(&a1));
}

// ---- PR #138 review: prune must not leave tips on deleted commits ----

/// Hard-reset `repo`'s current branch (and worktree) to `to`.
fn hard_reset(repo: &git2::Repository, to: git2::Oid) {
    let obj = repo.find_object(to, None).unwrap();
    repo.reset(&obj, git2::ResetType::Hard, None).unwrap();
}

/// Every tip of the store behind `path` names a commit the store still
/// holds: a tip on a pruned commit would claim coverage it no longer has.
async fn assert_no_tip_on_a_deleted_commit(path: &Path) {
    let stored = stored_commits(path).await;
    for (key, sha) in all_tips(path).await {
        assert!(
            stored.contains(&sha),
            "tip {key:?} points at pruned commit {sha}"
        );
    }
}

/// Main indexed at B, reset to A, pruned, then B restored from the reflog.
/// Returns `(main dir, a, b)` with HEAD back on B and B's rows pruned.
async fn main_reset_pruned_then_restored() -> (tempfile::TempDir, git2::Oid, git2::Oid) {
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    let repo = git2::Repository::open(main.path()).unwrap();
    let a = repo.head().unwrap().peel_to_commit().unwrap().id();
    let b = commit_file(&repo, "b.rs", "fn two() {}\n", "second");
    index_checkout(main.path()).await;

    hard_reset(&repo, a);
    let out = make_test_engine()
        .prune_unreachable(main.path())
        .await
        .expect("prune");
    assert_eq!(out.commits_deleted, 1, "B is unreachable after the reset");
    assert!(!stored_commits(main.path()).await.contains(&b.to_string()));
    assert_no_tip_on_a_deleted_commit(main.path()).await;
    assert_eq!(
        all_tips(main.path()).await,
        vec![(String::new(), a.to_string())]
    );

    hard_reset(&repo, b);
    (main, a, b)
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn daemon_reindexes_a_commit_restored_after_reset_and_prune() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, _a, b) = main_reset_pruned_then_restored().await;

    let (_rec, engine) = recording_engine();
    let out = engine
        .ensure_indexed(main.path(), &sha_of(b))
        .await
        .expect("ensure_indexed");
    assert!(out.indexed, "the pruned tip must not count as coverage");
    assert_eq!(out.new_commits, 1);
    assert!(stored_commits(main.path()).await.contains(&b.to_string()));
    assert_eq!(
        all_tips(main.path()).await,
        vec![(String::new(), b.to_string())]
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn incremental_index_reindexes_a_commit_restored_after_reset_and_prune() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, _a, b) = main_reset_pruned_then_restored().await;

    index_checkout(main.path()).await;
    assert!(stored_commits(main.path()).await.contains(&b.to_string()));
    assert_eq!(
        all_tips(main.path()).await,
        vec![(String::new(), b.to_string())]
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn prune_rewinds_a_live_worktree_tip_to_its_merge_base_with_head() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let (main, slot1) = repo_with_worktree().await;
    let wt = git2::Repository::open(&slot1).unwrap();
    let base = wt.head().unwrap().peel_to_commit().unwrap().id();
    let gone = commit_file(&wt, "c.rs", "fn three() {}\n", "slot only");
    index_checkout(&slot1).await;

    hard_reset(&wt, base);
    let out = make_test_engine()
        .prune_unreachable(main.path())
        .await
        .expect("prune");
    assert_eq!(out.commits_deleted, 1);
    assert!(!stored_commits(main.path())
        .await
        .contains(&gone.to_string()));
    assert_no_tip_on_a_deleted_commit(main.path()).await;
    assert_eq!(tip_of(&slot1).await, Some(base.to_string()));

    hard_reset(&wt, gone);
    index_checkout(&slot1).await;
    assert!(stored_commits(main.path())
        .await
        .contains(&gone.to_string()));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn prune_drops_a_main_tip_with_no_merge_base_and_the_next_daemon_pass_restores_it() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    let main = tempfile::tempdir().unwrap();
    build_test_repo(main.path());
    let repo = git2::Repository::open(main.path()).unwrap();
    let head = commit_file(&repo, "b.rs", "fn two() {}\n", "second");
    index_checkout(main.path()).await;

    // The main tip's commit object is gone (as after a reset and `git gc`):
    // there is no merge base to rewind to.
    let id = ohara_git::RepoIdentity::resolve(main.path()).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    let collected =
        ohara_core::CommitSha::parse("ffffffffffffffffffffffffffffffffffffffff").unwrap();
    ohara_core::TipStore::set_indexed_tip(
        &sqlite,
        &id.repo_id,
        &ohara_core::WorktreeKey::main(),
        &collected,
    )
    .await
    .unwrap();

    make_test_engine()
        .prune_unreachable(main.path())
        .await
        .expect("prune");
    assert_no_tip_on_a_deleted_commit(main.path()).await;

    let (_rec, engine) = recording_engine();
    let out = engine
        .ensure_indexed(main.path(), &sha_of(head))
        .await
        .expect("the next daemon pass re-establishes the main tip");
    assert!(out.indexed);
    assert_eq!(
        all_tips(main.path()).await,
        vec![(String::new(), head.to_string())]
    );
}
