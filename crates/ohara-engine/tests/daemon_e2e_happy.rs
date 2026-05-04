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
    let listener = tokio::spawn(async move {
        serve_unix(server_engine, &sock_for_task, server_stop).await
    });

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
    assert!(
        resp.error.is_none(),
        "shutdown must not error: {resp:?}"
    );

    tokio::time::timeout(Duration::from_secs(2), listener)
        .await
        .expect("listener task must exit within 2 s of Shutdown")
        .expect("listener task must not panic")
        .expect("serve_unix must return Ok");
}
