//! Plan 23 — sweep `RankingWeights::rerank_top_k` across {20, 50, 100,
//! 150, 200} on the deterministic context-engine eval fixture. Emits
//! one JSON-lines row per (pool_size, metrics) tuple on stderr and
//! prints a recommended default (smallest pool whose recall_at_5 is
//! within 1% of the best observed and whose p95_ms is within 1.5x of
//! the smallest observed) at the end.
//!
//! `#[ignore]`'d so the default `cargo test --workspace` stays fast.
//! Run with:
//!
//! ```sh
//! cargo test -p ohara-perf-tests -- --ignored rerank_pool_sweep --nocapture
//! ```
//!
//! Output is intended to be diff'd against the previous run (commit
//! the JSONL block to `tests/perf/baselines/rerank_pool_sweep.jsonl`
//! when the recommended default changes).
//!
//! The construction logic deliberately mirrors `context_engine_eval.rs`
//! verbatim per CONTRIBUTING.md: a perf runner is a standalone
//! operator tool and should be readable end-to-end without chasing
//! helpers across files.

use anyhow::{Context, Result};
use ohara_core::query::PatternQuery;
use ohara_core::retriever::RankingWeights;
use ohara_core::Retriever;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

const POOL_SIZES: &[usize] = &[20, 50, 100, 150, 200];

#[derive(Debug, Serialize)]
struct SweepRow {
    pool_size: usize,
    cases: usize,
    recall_at_1: f32,
    recall_at_5: f32,
    mrr: f32,
    p50_ms: f32,
    p95_ms: f32,
}

/// One row of `golden.jsonl`. Schema mirrors `context_engine_eval.rs`
/// verbatim — the perf-runner duplication is intentional per
/// CONTRIBUTING.md (each runner readable end-to-end).
#[derive(Debug, Deserialize)]
struct GoldenCase {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    query: String,
    #[allow(dead_code)]
    language: Option<String>,
    #[allow(dead_code)]
    since_unix: Option<i64>,
    /// Ordered by importance. Each label resolves to a SHA at runtime
    /// via the fixture's commit-message lookup.
    #[allow(dead_code)]
    expected_commit_labels: Vec<String>,
    /// Hint for failure-mode debugging. Not used in scoring.
    #[allow(dead_code)]
    expected_paths: Vec<String>,
    /// Plan 12 Task 2.2 metadata; not used by the sweep but parsed so
    /// the JSONL stays compatible with the eval runner.
    #[allow(dead_code)]
    #[serde(default)]
    expected_profile: Option<String>,
    #[allow(dead_code)]
    notes: String,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root resolves")
        .to_path_buf()
}

/// Build the deterministic eval fixture if it doesn't already exist.
/// Mirrors `context_engine_eval::ensure_fixture` verbatim — duplication
/// intentional per CONTRIBUTING.md (perf runners are operator tools).
fn ensure_fixture() -> Result<PathBuf> {
    let root = workspace_root();
    let script = root.join("tests/perf/build_context_eval_fixture.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .context("invoke build_context_eval_fixture.sh")?;
    anyhow::ensure!(status.success(), "fixture builder failed");
    let dest = root.join("target/perf-fixtures/context-engine-eval");
    anyhow::ensure!(
        dest.join(".git").is_dir(),
        "fixture missing after builder ran: {}",
        dest.display()
    );
    Ok(dest)
}

/// Maps `expected_commit_labels` -> exact commit message in the fixture.
/// Mirrors `context_engine_eval::LABEL_TO_MESSAGE` verbatim.
const LABEL_TO_MESSAGE: &[(&str, &str)] = &[
    ("initial_skeleton_commit", "initial: project skeleton"),
    ("readme_noise_commit", "docs: expand README"),
    ("timeout_commit", "fetch: add request timeout handling"),
    (
        "retry_backoff_commit",
        "fetch: add retry with exponential backoff",
    ),
    ("login_commit", "auth: introduce login function"),
    ("error_context_commit", "error: wrap errors with context"),
    ("logout_noise_commit", "auth: stub logout"),
    (
        "config_loader_commit",
        "config: load configuration from environment",
    ),
];

fn resolve_labels(fixture: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let raw = Command::new("git")
        .arg("-C")
        .arg(fixture)
        .args(["log", "--format=%H%x09%s"])
        .output()
        .context("git log against fixture")?;
    anyhow::ensure!(
        raw.status.success(),
        "git log failed: {}",
        String::from_utf8_lossy(&raw.stderr)
    );
    let stdout = String::from_utf8(raw.stdout).context("git log stdout utf8")?;
    let mut by_message: HashMap<&str, &str> = HashMap::new();
    for line in stdout.lines() {
        if let Some((sha, message)) = line.split_once('\t') {
            by_message.insert(message, sha);
        }
    }
    for (label, message) in LABEL_TO_MESSAGE {
        let sha = by_message.get(message).with_context(|| {
            format!(
                "fixture missing commit for label '{label}' (message=\"{message}\"); \
                 check tests/perf/build_context_eval_fixture.sh"
            )
        })?;
        map.insert((*label).to_string(), (*sha).to_string());
    }
    Ok(map)
}

/// Build storage + embedder + reranker once. They're identical across
/// pool sizes; only `RankingWeights::rerank_top_k` changes between
/// sweep iterations. Construction mirrors `context_engine_eval.rs` —
/// the plan deliberately specifies copy-not-abstract for perf runners.
async fn build_pipeline_components(
    db_path: PathBuf,
) -> Result<(
    Arc<dyn ohara_core::Storage>,
    Arc<dyn ohara_core::EmbeddingProvider>,
    Arc<dyn ohara_core::embed::RerankProvider>,
)> {
    let storage: Arc<dyn ohara_core::Storage> = Arc::new(
        ohara_storage::SqliteStorage::open(&db_path)
            .await
            .context("open eval index db")?,
    );
    let provider = ohara_cli::commands::provider::resolve_provider(
        ohara_cli::commands::provider::ProviderArg::Cpu,
    );
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedProvider::with_provider(provider)
        })
        .await??,
    );
    let reranker: Arc<dyn ohara_core::embed::RerankProvider> = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedReranker::with_provider(provider)
        })
        .await??,
    );
    Ok((storage, embedder, reranker))
}

fn load_golden() -> Result<Vec<GoldenCase>> {
    let path = workspace_root().join("tests/perf/fixtures/context_engine_eval/golden.jsonl");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read golden.jsonl at {}", path.display()))?;
    let mut cases = Vec::new();
    for (line_no, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: GoldenCase = serde_json::from_str(line)
            .with_context(|| format!("parse golden.jsonl line {}", line_no + 1))?;
        cases.push(case);
    }
    anyhow::ensure!(!cases.is_empty(), "golden.jsonl had no cases");
    Ok(cases)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf sweep; opt-in via `--ignored rerank_pool_sweep`"]
async fn rerank_pool_sweep() -> Result<()> {
    // Skeleton — body filled in by Tasks A.4 through A.6.
    let _cases = load_golden()?;
    let _ = (POOL_SIZES, std::any::type_name::<SweepRow>());
    let _ = std::any::type_name::<Retriever>();
    let _ = std::any::type_name::<RankingWeights>();
    let _ = std::any::type_name::<PatternQuery>();
    let _ = ensure_fixture;
    let _ = resolve_labels;
    let _ = build_pipeline_components;
    let _ = Instant::now();
    Ok(())
}
