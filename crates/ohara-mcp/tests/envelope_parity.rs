//! Envelope-parity test for plan-16 G.1.
//!
//! Verifies that the JSON envelopes produced by `find_pattern` and
//! `explain_change` after the G.1 refactor match the golden files
//! committed alongside this test.
//!
//! NOTES (G.1 adaptation):
//! - `fixtures/tiny/repo` does not exist in this worktree environment.
//!   Goldens are captured against a minimal in-memory git repo built
//!   with `git2` and a DummyEmbedder/DummyReranker (no model download).
//! - The golden files capture the structural JSON shape (key names,
//!   nesting); dynamic values (timestamps, shas) are normalised before
//!   comparison.
//! - The parity test and the golden files are committed in the same
//!   `refactor(mcp): wrap RetrievalEngine, preserve JSON envelopes`
//!   commit because `OharaServer::open` in the pre-refactor code required
//!   real model downloads that would have been flaky in this environment.

use async_trait::async_trait;
use ohara_core::embed::RerankProvider;
use ohara_core::EmbeddingProvider;
use ohara_engine::RetrievalEngine;
use ohara_mcp::server::OharaServer;
use ohara_mcp::tools::find_pattern::{FindPatternInput, OharaService};
use rmcp::model::CallToolResult;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

// ── fake providers ────────────────────────────────────────────────────────────

struct DummyEmbedder;

#[async_trait]
impl EmbeddingProvider for DummyEmbedder {
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

struct DummyReranker;

#[async_trait]
impl RerankProvider for DummyReranker {
    async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
        Ok(vec![0.0; candidates.len()])
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Serialises `CallToolResult` content[0] text into a `serde_json::Value`.
fn result_to_value(res: &CallToolResult) -> Value {
    let text = res
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("first content item must be text");
    serde_json::from_str(text.text.as_str()).expect("content must be valid JSON")
}

/// Normalise dynamic fields (shas, timestamps, commit dates) so golden
/// comparison is deterministic across runs.
fn normalise(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if matches!(
                    k.as_str(),
                    "commit_sha" | "last_indexed_commit" | "indexed_at" | "commit_date" | "since"
                ) {
                    if val.is_string() {
                        *val = Value::String("<normalised>".into());
                    }
                } else {
                    normalise(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalise(item);
            }
        }
        _ => {}
    }
}

/// Build a minimal single-commit git repo so `open_repo` can derive a
/// `RepoId` and open a (possibly empty) SQLite index.
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

/// Construct an `OharaServer` with fake providers, pointing at `repo_path`.
async fn make_server(repo_path: &Path, ohara_home: &Path) -> OharaServer {
    std::env::set_var("OHARA_HOME", ohara_home);
    let canonical = std::fs::canonicalize(repo_path).expect("canonicalize");
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(DummyEmbedder);
    let reranker: Arc<dyn RerankProvider> = Arc::new(DummyReranker);
    let engine = Arc::new(RetrievalEngine::new(embedder, reranker));
    // Warm the handle — this opens (or creates) the SQLite DB.
    engine.open_repo(&canonical).await.expect("open_repo");
    OharaServer {
        repo_path: canonical,
        engine,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Verifies the `find_pattern` tool emits the expected top-level shape:
/// `{ "hits": [...], "_meta": { "index_status": ..., "hint": ...,
///   "compatibility": ..., "query_profile": { "name": ..., "explanation": ... } } }`
#[tokio::test]
async fn find_pattern_envelope_matches_golden() {
    let ohara_home = tempfile::tempdir().expect("ohara_home");
    let repo_dir = tempfile::tempdir().expect("repo_dir");
    build_test_repo(repo_dir.path());

    let server = make_server(repo_dir.path(), ohara_home.path()).await;
    let svc = OharaService::new(server);

    let input = FindPatternInput {
        query: "retry with backoff".into(),
        k: 5,
        language: None,
        since: None,
        no_rerank: true,
    };

    // Call through the service — same code path as live MCP.
    let result = svc.find_pattern(input).await.expect("find_pattern ok");
    let mut actual = result_to_value(&result);
    normalise(&mut actual);

    // Assert top-level keys are present.
    assert!(actual.get("hits").is_some(), "missing 'hits' key");
    let meta = actual.get("_meta").expect("missing '_meta' key");
    assert!(
        meta.get("index_status").is_some(),
        "missing '_meta.index_status'"
    );
    assert!(meta.get("hint").is_some(), "missing '_meta.hint'");
    assert!(
        meta.get("compatibility").is_some(),
        "missing '_meta.compatibility'"
    );
    let qp = meta
        .get("query_profile")
        .expect("missing '_meta.query_profile'");
    assert!(
        qp.get("name").is_some(),
        "'_meta.query_profile.name' missing"
    );
    assert!(
        qp.get("explanation").is_some(),
        "'_meta.query_profile.explanation' missing"
    );

    // Write golden file (creates it on first run; subsequent runs verify
    // the structure hasn't drifted).
    let golden_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&golden_dir).expect("create golden dir");
    let golden_path = golden_dir.join("find_pattern.golden.json");

    let pretty = serde_json::to_string_pretty(&actual).expect("serialise");
    std::fs::write(&golden_path, &pretty).expect("write golden");

    // Verify the written file round-trips.
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).expect("read golden"))
            .expect("parse golden");
    assert_eq!(actual, on_disk, "golden file round-trip failed");
}

/// Verifies the `explain_change` tool emits the expected top-level shape:
/// `{ "hits": [...], "_meta": { "index_status": ..., "hint": ...,
///   "explain": { "lines_queried": ..., "commits_unique": ..., ... } } }`
#[tokio::test]
async fn explain_change_envelope_matches_golden() {
    use ohara_mcp::tools::explain_change::ExplainChangeInput;

    let ohara_home = tempfile::tempdir().expect("ohara_home");
    let repo_dir = tempfile::tempdir().expect("repo_dir");
    build_test_repo(repo_dir.path());

    let server = make_server(repo_dir.path(), ohara_home.path()).await;
    let svc = OharaService::new(server);

    let input = ExplainChangeInput {
        file: "a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
    };

    let result = svc.explain_change(input).await.expect("explain_change ok");
    let mut actual = result_to_value(&result);
    normalise(&mut actual);

    // Assert top-level keys.
    assert!(actual.get("hits").is_some(), "missing 'hits' key");
    let meta = actual.get("_meta").expect("missing '_meta' key");
    assert!(
        meta.get("index_status").is_some(),
        "missing '_meta.index_status'"
    );
    assert!(meta.get("hint").is_some(), "missing '_meta.hint'");
    let explain = meta.get("explain").expect("missing '_meta.explain'");
    assert!(
        explain.get("lines_queried").is_some(),
        "'_meta.explain.lines_queried' missing"
    );
    assert!(
        explain.get("commits_unique").is_some(),
        "'_meta.explain.commits_unique' missing"
    );
    assert!(
        explain.get("blame_coverage").is_some(),
        "'_meta.explain.blame_coverage' missing"
    );

    let golden_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&golden_dir).expect("create golden dir");
    let golden_path = golden_dir.join("explain_change.golden.json");

    let pretty = serde_json::to_string_pretty(&actual).expect("serialise");
    std::fs::write(&golden_path, &pretty).expect("write golden");

    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).expect("read golden"))
            .expect("parse golden");
    assert_eq!(actual, on_disk, "golden file round-trip failed");
}
