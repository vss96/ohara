//! Plan 10 Task 2.1 — context-engine retrieval-quality runner.
//!
//! Builds the deterministic eval fixture, indexes it through the same
//! `ohara_cli::commands::index::run` path the binary uses, then runs
//! every case from `fixtures/context_engine_eval/golden.jsonl` through
//! the same `Retriever::find_pattern` path the CLI / MCP use. Emits a
//! one-line JSON metrics summary on stderr (`cases`, `recall_at_1`,
//! `recall_at_5`, `mrr`, `ndcg_lite`, `p50_ms`, `p95_ms`, `failed_ids`)
//! and prints a per-failed-case dump (top hits, paths, scores) so a
//! regression is debuggable from the test output alone.
//!
//! `#[ignore]`'d so the default `cargo test --workspace` stays fast.
//! Run with:
//!
//! ```sh
//! cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture
//! ```
//!
//! Initial pass thresholds (Plan 10 Task 2.1 Step 3):
//! - `recall_at_5 == 1.0` (every case must hit at least one expected
//!   commit in the top 5)
//! - `mrr >= 0.80`
//! - no individual query exceeds 2 s wall-time on the tiny fixture
//!
//! Latency is a smoke signal, not the contract — the runner records
//! p50/p95 for trend-watching.

use anyhow::{Context, Result};
use ohara_core::query::PatternQuery;
use ohara_core::Retriever;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

/// Exact rerun command for failure messages — kept as a constant so
/// the assertion text and the per-failure dump stay in sync.
const RERUN_CMD: &str =
    "cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture";

/// One row of `golden.jsonl`. Fields mirror the schema documented in
/// `tests/perf/README.md` and `fixtures/context_engine_eval/README.md`.
#[derive(Debug, Deserialize)]
struct GoldenCase {
    id: String,
    query: String,
    language: Option<String>,
    since_unix: Option<i64>,
    /// Ordered by importance. Each label resolves to a SHA at runtime
    /// via the fixture's commit-message lookup so the JSONL doesn't pin
    /// hashes that change every time the script is edited.
    expected_commit_labels: Vec<String>,
    /// Hint for failure-mode debugging. Not used in scoring today;
    /// printed alongside actual paths when a case fails.
    #[allow(dead_code)]
    expected_paths: Vec<String>,
    #[allow(dead_code)]
    notes: String,
}

/// Output line emitted to stderr after a run. One JSON object per
/// invocation so callers (PR descriptions, CI annotations) can paste
/// the line verbatim.
#[derive(Debug, Serialize)]
struct EvalSummary {
    cases: usize,
    recall_at_1: f64,
    recall_at_5: f64,
    mrr: f64,
    ndcg_lite: f64,
    p50_ms: u128,
    p95_ms: u128,
    failed_ids: Vec<String>,
}

/// Per-case result captured during the run. Kept for the failure dump
/// even on cases that scored fine, so debugging is uniform.
struct CaseResult {
    id: String,
    expected_shas: Vec<String>,
    hits: Vec<ohara_core::query::PatternHit>,
    elapsed_ms: u128,
    /// 1-based rank of the first expected SHA in `hits`, or `None` if
    /// no expected SHA appeared in the top-K.
    first_hit_rank: Option<usize>,
}

/// Maps `expected_commit_labels` -> exact commit message in the fixture.
/// Anchored here (not in the JSONL) because the labels are runner-side
/// indirection, while the messages are the fixture script's contract.
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

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root resolves")
        .to_path_buf()
}

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

/// Build `label -> sha` from the fixture's git log. Walks the message
/// table once and asks `git log` for each one. The fixture script
/// guarantees commit-message uniqueness, so a label maps to exactly
/// one SHA when present.
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

fn percentile(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank: index = ceil(pct/100 * n) - 1, clamped to [0, n-1].
    let n = sorted.len();
    let idx_f = ((pct / 100.0) * n as f64).ceil() as usize;
    let idx = idx_f.saturating_sub(1).min(n - 1);
    sorted[idx]
}

fn dcg_at_rank(rank: usize) -> f64 {
    // Standard nDCG with binary relevance and a single relevant item:
    // gain=1, so DCG = 1 / log2(rank + 1). IDCG (best case = rank 1)
    // is 1.0, so nDCG_lite per case == 1 / log2(rank + 1).
    1.0 / ((rank as f64) + 1.0).log2()
}

fn summarise(results: &[CaseResult]) -> EvalSummary {
    let cases = results.len();
    let mut hit_at_1 = 0usize;
    let mut hit_at_5 = 0usize;
    let mut mrr_sum = 0.0;
    let mut ndcg_sum = 0.0;
    let mut failed_ids = Vec::new();
    let mut latencies: Vec<u128> = Vec::with_capacity(cases);

    for r in results {
        latencies.push(r.elapsed_ms);
        match r.first_hit_rank {
            Some(rank) => {
                if rank == 1 {
                    hit_at_1 += 1;
                }
                if rank <= 5 {
                    hit_at_5 += 1;
                } else {
                    failed_ids.push(r.id.clone());
                }
                mrr_sum += 1.0 / (rank as f64);
                ndcg_sum += dcg_at_rank(rank);
            }
            None => {
                failed_ids.push(r.id.clone());
            }
        }
    }

    latencies.sort_unstable();
    let p50_ms = percentile(&latencies, 50.0);
    let p95_ms = percentile(&latencies, 95.0);

    let denom = cases.max(1) as f64;
    EvalSummary {
        cases,
        recall_at_1: hit_at_1 as f64 / denom,
        recall_at_5: hit_at_5 as f64 / denom,
        mrr: mrr_sum / denom,
        ndcg_lite: ndcg_sum / denom,
        p50_ms,
        p95_ms,
        failed_ids,
    }
}

fn print_failure_dump(results: &[CaseResult], summary: &EvalSummary, cases: &[GoldenCase]) {
    if summary.failed_ids.is_empty() {
        return;
    }
    eprintln!("\n--- failed cases ---");
    for failed_id in &summary.failed_ids {
        let result = match results.iter().find(|r| &r.id == failed_id) {
            Some(r) => r,
            None => continue,
        };
        let case = match cases.iter().find(|c| &c.id == failed_id) {
            Some(c) => c,
            None => continue,
        };
        eprintln!("\n  id: {}", failed_id);
        eprintln!("  query: {:?}", case.query);
        eprintln!("  expected_shas: {:?}", result.expected_shas);
        eprintln!("  expected_paths: {:?}", case.expected_paths);
        eprintln!("  first_hit_rank: {:?}", result.first_hit_rank);
        eprintln!("  elapsed_ms: {}", result.elapsed_ms);
        eprintln!("  top hits (lane_debug: null — runner doesn't surface lane contributions yet):");
        for (i, hit) in result.hits.iter().enumerate().take(5) {
            let msg_first_line = hit.commit_message.lines().next().unwrap_or("");
            eprintln!(
                "    {}. sha={} score={:.4} provenance={:?} path={} message={:?}",
                i + 1,
                hit.commit_sha,
                hit.combined_score,
                hit.provenance,
                hit.file_path,
                msg_first_line,
            );
        }
    }
    eprintln!("\nrerun:\n  {RERUN_CMD}\n");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "retrieval-quality eval — opt in via --include-ignored, downloads embedder + reranker on first run"]
async fn context_engine_eval_passes_thresholds() -> Result<()> {
    let fixture = ensure_fixture()?;
    let label_to_sha = resolve_labels(&fixture)?;
    let cases = load_golden()?;

    // Isolate the index DB so the eval doesn't pollute (or get
    // polluted by) a developer's real ~/.ohara state.
    let home = tempfile::tempdir().context("temp OHARA_HOME")?;
    std::env::set_var("OHARA_HOME", home.path());

    // Index the fixture through the same path the binary uses, so the
    // chunker / hunk-text / FTS / vector wiring matches production.
    let index_args = ohara_cli::commands::index::Args {
        path: fixture.clone(),
        incremental: false,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(64),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Cpu),
        resources: ohara_cli::resources::ResourcesArg::Auto,
    };
    ohara_cli::commands::index::run(index_args)
        .await
        .context("index eval fixture")?;

    // Build a Retriever with the same components the CLI uses.
    let (repo_id, _, _) =
        ohara_cli::commands::resolve_repo_id(&fixture).context("resolve repo id")?;
    let db_path = ohara_cli::commands::index_db_path(&repo_id).context("resolve db path")?;
    let storage = Arc::new(
        ohara_storage::SqliteStorage::open(&db_path)
            .await
            .context("open eval index db")?,
    );
    let provider = ohara_cli::commands::provider::resolve_provider(
        ohara_cli::commands::provider::ProviderArg::Cpu,
    );
    let embedder = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedProvider::with_provider(provider)
        })
        .await??,
    );
    let reranker = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedReranker::with_provider(provider)
        })
        .await??,
    );
    let retriever = Retriever::new(storage, embedder).with_reranker(reranker);

    // Use a fixed "now" so the recency multiplier is stable across runs.
    // Pick a timestamp slightly after the fixture's last commit
    // (2024-06-01) so all commits register as "recent enough" but the
    // ordering across them is still meaningful.
    let now_unix = chrono::DateTime::parse_from_rfc3339("2024-07-01T00:00:00Z")
        .expect("static rfc3339 literal parses")
        .timestamp();

    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        let expected_shas: Vec<String> = case
            .expected_commit_labels
            .iter()
            .map(|label| {
                label_to_sha
                    .get(label)
                    .cloned()
                    .unwrap_or_else(|| panic!("unknown label '{label}' in case {}", case.id))
            })
            .collect();

        let q = PatternQuery {
            query: case.query.clone(),
            k: 5,
            language: case.language.clone(),
            since_unix: case.since_unix,
            no_rerank: false,
        };
        let start = Instant::now();
        let hits = retriever
            .find_pattern(&repo_id, &q, now_unix)
            .await
            .with_context(|| format!("find_pattern for case {}", case.id))?;
        let elapsed_ms = start.elapsed().as_millis();

        let first_hit_rank = expected_shas.iter().find_map(|expected| {
            hits.iter()
                .position(|h| &h.commit_sha == expected)
                .map(|i| i + 1)
        });

        results.push(CaseResult {
            id: case.id.clone(),
            expected_shas,
            hits,
            elapsed_ms,
            first_hit_rank,
        });
    }

    let summary = summarise(&results);
    let summary_json = serde_json::to_string(&summary).context("serialize EvalSummary")?;
    eprintln!("perf::context_engine_eval {summary_json}");

    print_failure_dump(&results, &summary, &cases);

    let max_ms = results.iter().map(|r| r.elapsed_ms).max().unwrap_or(0);

    // Hard contracts (Plan 10 Task 2.1 Step 3). Each failure message
    // includes the rerun command so a CI log truncated to the assertion
    // line still tells operators how to reproduce locally.
    assert!(
        (summary.recall_at_5 - 1.0).abs() < f64::EPSILON,
        "recall_at_5 must be 1.0; got {} (failed_ids={:?}). Rerun: {RERUN_CMD}",
        summary.recall_at_5,
        summary.failed_ids,
    );
    assert!(
        summary.mrr >= 0.80,
        "mrr must be >= 0.80; got {} (failed_ids={:?}). Rerun: {RERUN_CMD}",
        summary.mrr,
        summary.failed_ids,
    );
    assert!(
        max_ms < 2000,
        "individual query exceeded the 2 s smoke threshold: {max_ms} ms. Rerun: {RERUN_CMD}",
    );

    Ok(())
}
