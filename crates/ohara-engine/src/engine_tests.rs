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
        let commit_src = ohara_git::GitCommitSource::open(&canonical).unwrap();
        let symbol_src = ohara_parse::GitSymbolSource::open(&canonical).unwrap();
        let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
        indexer
            .run(&repo_id, &commit_src, &symbol_src)
            .await
            .unwrap();
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

#[tokio::test]
async fn find_pattern_returns_empty_hits_on_empty_index() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    {
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", ohara_home.path());
    }
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

#[tokio::test]
async fn open_repo_caches_handle_by_repo_id() {
    // `env_lock` serialises tests that mutate OHARA_HOME (process-global).
    // Drop the guard before the first await to satisfy clippy::await_holding_lock.
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    {
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", ohara_home.path());
    }
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
        let commit_src = ohara_git::GitCommitSource::open(&canonical).unwrap();
        let symbol_src = ohara_parse::GitSymbolSource::open(&canonical).unwrap();
        let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
        indexer
            .run(&repo_id, &commit_src, &symbol_src)
            .await
            .unwrap();
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
