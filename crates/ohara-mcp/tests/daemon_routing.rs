//! Plan-29: the MCP tools route through a daemon when one is reachable.
//! Spins a real `serve_unix` listener with stub providers and points the
//! thin client at it via OHARA_DAEMON_SOCKET.
//!
//! IMPORTANT: env vars are process-global. Each integration-test file is a
//! separate test binary, so this file's `OHARA_DAEMON_SOCKET` / `OHARA_HOME`
//! mutation cannot leak into `envelope_parity.rs` (a different process). To
//! keep this file's own state coherent it contains exactly ONE test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use ohara_core::embed::RerankProvider;
use ohara_core::EmbeddingProvider;
use ohara_engine::{serve_unix, RetrievalEngine};
use ohara_mcp::tools::explain_change::ExplainChangeInput;
use ohara_mcp::tools::find_pattern::{FindPatternInput, OharaService};
use rmcp::model::CallToolResult;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── stub providers (mirror envelope_parity.rs) ────────────────────────────────

struct StubEmbedder;

#[async_trait]
impl EmbeddingProvider for StubEmbedder {
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

struct StubReranker;

#[async_trait]
impl RerankProvider for StubReranker {
    async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
        Ok(vec![0.0; candidates.len()])
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Serialise `CallToolResult` content[0] text into a `serde_json::Value`.
fn result_to_value(res: &CallToolResult) -> Value {
    let text = res
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("first content item must be text");
    serde_json::from_str(text.text.as_str()).expect("content must be valid JSON")
}

/// Build a minimal single-commit git repo (mirror envelope_parity.rs).
fn build_test_repo(dir: &Path) {
    use git2::{Repository, Signature};
    let repo = Repository::init(dir).expect("init");
    std::fs::write(dir.join("a.rs"), "fn one() {}\n").expect("write");
    let sig = Signature::now("a", "a@a").expect("sig");
    let mut idx = repo.index().expect("index");
    idx.add_path(Path::new("a.rs")).expect("add");
    idx.write().expect("write index");
    let tree_id = idx.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
        .expect("commit");
}

/// Index `canonical` so storage carries commit metadata (mirror the
/// engine_tests indexing block).
async fn index_repo(canonical: &Path) {
    let walker = ohara_git::GitWalker::open(canonical).expect("walker");
    let first = walker.first_commit_sha().expect("first_commit_sha");
    let repo_id = ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
    let db_path = ohara_core::paths::index_db_path(&repo_id).expect("db_path");
    let storage: Arc<dyn ohara_core::Storage> = Arc::new(
        ohara_storage::SqliteStorage::open(&db_path)
            .await
            .expect("open"),
    );
    let commit_src = Arc::new(ohara_git::GitCommitSource::open(canonical).expect("commit_src"));
    let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(canonical).expect("symbol_src"));
    let indexer = ohara_core::Indexer::new(storage, Arc::new(StubEmbedder));
    indexer
        .run(&repo_id, commit_src, symbol_src)
        .await
        .expect("index");
}

// ── test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_tools_route_through_daemon_socket() {
    let ohara_home = tempfile::tempdir().expect("ohara_home");
    let repo_dir = tempfile::tempdir().expect("repo_dir");
    let sock_dir = tempfile::tempdir().expect("sock_dir");
    std::env::set_var("OHARA_HOME", ohara_home.path());

    build_test_repo(repo_dir.path());
    let canonical = std::fs::canonicalize(repo_dir.path()).expect("canonicalize");
    index_repo(&canonical).await;

    // Spawn a real daemon on a temp socket with the stub-provider engine.
    let engine = Arc::new(RetrievalEngine::new(
        Arc::new(StubEmbedder),
        Arc::new(StubReranker),
    ));
    let sock = sock_dir.path().join("ohara.sock");
    let stop = CancellationToken::new();
    let daemon = {
        let s = sock.clone();
        let stop2 = stop.clone();
        tokio::spawn(async move { serve_unix(engine, &s, stop2).await })
    };

    // Wait for the socket file to appear (up to 1 s).
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(sock.exists(), "daemon socket never appeared at {sock:?}");

    std::env::set_var("OHARA_DAEMON_SOCKET", &sock);

    let server = ohara_mcp::server::OharaServer::open(&canonical).unwrap();
    let svc = OharaService::new(server);

    // find_pattern routes through the daemon (FindPattern IPC).
    let fp = svc
        .find_pattern(FindPatternInput {
            query: "one".into(),
            k: 5,
            language: None,
            since: None,
            no_rerank: true,
        })
        .await
        .expect("find_pattern ok");
    let fp_val = result_to_value(&fp);
    assert!(fp_val.get("hits").is_some(), "missing 'hits': {fp_val}");
    let fp_meta = fp_val.get("_meta").expect("missing '_meta'");
    assert!(
        fp_meta.get("index_status").is_some(),
        "missing '_meta.index_status': {fp_val}"
    );

    // explain_change exercises the two-IPC-call path (ExplainChange + IndexStatus).
    let ex = svc
        .explain_change(ExplainChangeInput {
            file: "a.rs".into(),
            line_start: 1,
            line_end: 1,
            k: 5,
            include_diff: false,
        })
        .await
        .expect("explain_change ok");
    let ex_val = result_to_value(&ex);
    let hits = ex_val
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("'hits' must be an array");
    assert!(!hits.is_empty(), "explain hits must be non-empty: {ex_val}");
    let ex_meta = ex_val.get("_meta").expect("missing '_meta'");
    assert!(
        ex_meta.get("index_status").is_some(),
        "missing '_meta.index_status': {ex_val}"
    );

    std::env::remove_var("OHARA_DAEMON_SOCKET");
    stop.cancel();
    let _ = daemon.await;
}
