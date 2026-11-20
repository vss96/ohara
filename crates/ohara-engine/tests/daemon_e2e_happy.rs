//! Plan-16 I.1: end-to-end happy-path test for the ohara daemon stack.
//!
//! Runs in-process — listener is a tokio task, client is the same-process
//! [`Client`]. Marked `#[ignore]` because the full lifecycle (socket bind,
//! git walk, storage open) takes several seconds.
//!
//! Run manually with:
//!   cargo test -p ohara-engine --test daemon_e2e_happy -- --ignored

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ohara_engine::client::Client;
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::server::serve_unix;
use ohara_engine::RetrievalEngine;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Dummy embedding / reranking stubs (no ML dependencies in integration tests)
// ---------------------------------------------------------------------------

struct DummyEmbedder;

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for DummyEmbedder {
    fn dimension(&self) -> usize {
        384
    }

    fn model_id(&self) -> &str {
        "dummy-i1"
    }

    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}

struct DummyReranker;

#[async_trait::async_trait]
impl ohara_core::embed::RerankProvider for DummyReranker {
    async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
        Ok(vec![0.0; candidates.len()])
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialise tests that mutate `OHARA_HOME` (a process-global env var).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn build_test_repo(dir: &Path) {
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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Full in-process daemon lifecycle:
///   spawn server → ping → find_pattern → invalidate_repo → find_pattern → shutdown.
///
/// Each step is a separate `Client::call` so the wire path (frame encode →
/// socket write → server dispatch → frame decode) is exercised end-to-end.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "in-process daemon e2e; run with: cargo test -p ohara-engine --test daemon_e2e_happy -- --ignored"]
async fn daemon_lifecycle_ping_query_invalidate_query_shutdown() {
    // Serialise OHARA_HOME mutations; held across all awaits intentionally.
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());

    let repo_dir = tempfile::tempdir().unwrap();
    build_test_repo(repo_dir.path());

    // Build engine with dummy providers.
    let engine = Arc::new(RetrievalEngine::new(
        Arc::new(DummyEmbedder),
        Arc::new(DummyReranker),
    ));

    // Bind the Unix socket in a background task.
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("ohara.sock");
    let stop = CancellationToken::new();

    let server_engine = engine.clone();
    let server_stop = stop.clone();
    let sock_for_task = sock.clone();
    let listener =
        tokio::spawn(async move { serve_unix(server_engine, &sock_for_task, server_stop).await });

    // Wait up to 1 s for the socket file to appear.
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sock.exists(), "socket must exist after server starts");

    let client = Client::connect(&sock);
    let repo_path = repo_dir.path().to_string_lossy().into_owned();

    // 1. Ping — must succeed with a result and no error.
    let resp = client
        .call(Request {
            id: 1,
            repo_path: None,
            method: RequestMethod::Ping,
        })
        .await
        .expect("ping call");
    assert!(resp.error.is_none(), "ping must not error: {resp:?}");
    assert!(resp.result.is_some(), "ping must carry a result: {resp:?}");

    // 2. find_pattern on an empty index — must return empty hits, no error.
    let q = ohara_core::query::PatternQuery {
        query: "hello".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let resp = client
        .call(Request {
            id: 2,
            repo_path: Some(repo_path.clone()),
            method: RequestMethod::FindPattern(q.clone()),
        })
        .await
        .expect("find_pattern call");
    assert!(
        resp.error.is_none(),
        "find_pattern on empty index must not error: {resp:?}"
    );
    assert!(
        resp.result.is_some(),
        "find_pattern must carry a result: {resp:?}"
    );

    // 3. invalidate_repo — evicts the cached handle; must succeed.
    let resp = client
        .call(Request {
            id: 3,
            repo_path: Some(repo_path.clone()),
            method: RequestMethod::InvalidateRepo,
        })
        .await
        .expect("invalidate_repo call");
    assert!(
        resp.error.is_none(),
        "invalidate_repo must not error: {resp:?}"
    );

    // 4. find_pattern again after invalidate — must still succeed (re-opens handle).
    let resp = client
        .call(Request {
            id: 4,
            repo_path: Some(repo_path),
            method: RequestMethod::FindPattern(q),
        })
        .await
        .expect("find_pattern-2 call");
    assert!(
        resp.error.is_none(),
        "find_pattern after invalidate must not error: {resp:?}"
    );

    // 5. Shutdown — daemon must acknowledge and stop within 2 s.
    let resp = client
        .call(Request {
            id: 5,
            repo_path: None,
            method: RequestMethod::Shutdown,
        })
        .await
        .expect("shutdown call");
    assert!(resp.error.is_none(), "shutdown must not error: {resp:?}");

    tokio::time::timeout(Duration::from_secs(2), listener)
        .await
        .expect("listener task must exit within 2 s of Shutdown")
        .expect("listener task must not panic")
        .expect("serve_unix must return Ok");
}

// ---------------------------------------------------------------------------
// plan-33: EnsureIndexed over the socket
// ---------------------------------------------------------------------------

/// Embedder reporting the default model id, so an index seeded with it is
/// `Compatible` with the daemon's engine (which is built from the same one).
struct DefaultModelEmbedder;

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for DefaultModelEmbedder {
    fn dimension(&self) -> usize {
        384
    }

    fn model_id(&self) -> &str {
        ohara_embed::DEFAULT_MODEL_ID
    }

    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}

/// Commit `body` as `file` on the current HEAD of the repo at `dir`.
fn commit_file(dir: &Path, file: &str, body: &str, msg: &str) -> git2::Oid {
    let repo = git2::Repository::open(dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(file)).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("a", "a@a").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
        .unwrap()
}

/// Index the checkout at `path` into the shared store (what `ohara index`
/// does before a daemon is ever involved).
async fn seed_index(path: &Path) {
    let id = ohara_git::RepoIdentity::resolve(path).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    let sqlite = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
    ohara_core::Storage::open_repo(
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
    let indexer = ohara_core::Indexer::new(sqlite.clone(), Arc::new(DefaultModelEmbedder))
        .with_runtime_metadata(ohara_engine::current_runtime_metadata(
            ohara_core::EmbedMode::Semantic,
        ))
        .with_tip_context(ohara_core::TipContext {
            tips: sqlite.clone(),
            worktree: id.worktree.clone(),
            head: ohara_core::CommitSha::parse(&head).unwrap(),
        });
    indexer
        .run(
            &id.repo_id,
            Arc::new(ohara_git::GitCommitSource::open(&id.worktree_path).unwrap()),
            Arc::new(ohara_parse::GitSymbolSource::open(&id.worktree_path).unwrap()),
        )
        .await
        .unwrap();
}

/// `ohara index --incremental` hands the work to a running daemon with
/// `EnsureIndexed { head }`; this drives that exact request over the wire.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ensure_indexed_over_the_socket_indexes_a_new_commit() {
    let _g = env_lock();
    let ohara_home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());

    let repo_dir = tempfile::tempdir().unwrap();
    build_test_repo(repo_dir.path());
    seed_index(repo_dir.path()).await;
    let new = commit_file(
        repo_dir.path(),
        "zebra.rs",
        "fn zebra_marker() {}\n",
        "add zebra_marker",
    );

    let engine = Arc::new(RetrievalEngine::new(
        Arc::new(DefaultModelEmbedder),
        Arc::new(DummyReranker),
    ));
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("ohara.sock");
    let stop = CancellationToken::new();
    let (e, s, p) = (engine.clone(), stop.clone(), sock.clone());
    let listener = tokio::spawn(async move { serve_unix(e, &p, s).await });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sock.exists(), "socket must exist after server starts");

    let client = Client::connect(&sock);
    let repo_path = repo_dir.path().to_string_lossy().into_owned();
    let resp = client
        .call_with_timeout(
            Request {
                id: 1,
                repo_path: Some(repo_path.clone()),
                method: RequestMethod::EnsureIndexed {
                    sha: new.to_string(),
                },
            },
            Duration::from_secs(60),
        )
        .await
        .expect("ensure_indexed call");
    assert!(
        resp.error.is_none(),
        "ensure_indexed must not error: {resp:?}"
    );
    let out: ohara_engine::EnsureIndexedOutcome =
        serde_json::from_value(resp.result.expect("result")).unwrap();
    assert!(out.indexed, "an uncovered head must run an index pass");
    assert_eq!(out.new_commits, 1);

    let resp = client
        .call(Request {
            id: 2,
            repo_path: Some(repo_path),
            method: RequestMethod::FindPattern(ohara_core::query::PatternQuery {
                query: "zebra_marker".into(),
                k: 5,
                language: None,
                since_unix: None,
                no_rerank: true,
            }),
        })
        .await
        .expect("find_pattern call");
    assert!(
        resp.error.is_none(),
        "find_pattern must not error: {resp:?}"
    );
    let found: ohara_engine::FindPatternResult =
        serde_json::from_value(resp.result.expect("result")).unwrap();
    assert!(
        found.hits.iter().any(|h| h.commit_sha == new.to_string()),
        "the new commit must be retrievable, got {:?}",
        found.hits.iter().map(|h| &h.commit_sha).collect::<Vec<_>>()
    );

    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;
}
