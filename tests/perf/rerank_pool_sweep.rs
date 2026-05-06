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
    id: String,
    query: String,
    language: Option<String>,
    since_unix: Option<i64>,
    /// Ordered by importance. Each label resolves to a SHA at runtime
    /// via the fixture's commit-message lookup.
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

fn percentile(sorted: &[f32], pct: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let idx_f = ((pct / 100.0) * n as f32).ceil() as usize;
    let idx = idx_f.saturating_sub(1).min(n - 1);
    sorted[idx]
}

async fn run_sweep_iteration(
    retriever: &Retriever,
    repo_id: &ohara_core::types::RepoId,
    cases: &[GoldenCase],
    label_to_sha: &HashMap<String, String>,
    now_unix: i64,
    pool: usize,
) -> Result<SweepRow> {
    let mut latencies_ms: Vec<f32> = Vec::with_capacity(cases.len());
    let mut hits_at_1: usize = 0;
    let mut hits_at_5: usize = 0;
    let mut rr_sum: f32 = 0.0;

    for case in cases {
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
        let started = Instant::now();
        let (hits, _profile) = retriever
            .find_pattern_with_profile(repo_id, &q, now_unix)
            .await
            .with_context(|| format!("find_pattern {} (pool={pool})", case.id))?;
        let elapsed_ms = started.elapsed().as_secs_f32() * 1000.0;
        latencies_ms.push(elapsed_ms);

        let any_in_top1 = hits
            .first()
            .map(|h| expected_shas.iter().any(|sha| sha == &h.commit_sha))
            .unwrap_or(false);
        if any_in_top1 {
            hits_at_1 += 1;
        }
        let first_rank = hits
            .iter()
            .take(5)
            .position(|h| expected_shas.iter().any(|sha| sha == &h.commit_sha));
        if first_rank.is_some() {
            hits_at_5 += 1;
        }
        if let Some(rank) = first_rank {
            rr_sum += 1.0 / ((rank + 1) as f32);
        }
    }

    latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile(&latencies_ms, 50.0);
    let p95 = percentile(&latencies_ms, 95.0);
    let denom = cases.len().max(1) as f32;
    Ok(SweepRow {
        pool_size: pool,
        cases: cases.len(),
        recall_at_1: hits_at_1 as f32 / denom,
        recall_at_5: hits_at_5 as f32 / denom,
        mrr: rr_sum / denom,
        p50_ms: p50,
        p95_ms: p95,
    })
}

/// Pick the smallest pool whose `recall_at_5` is within 1% of the best
/// observed AND whose `p95_ms` is within 1.5x of the smallest observed.
/// "Smallest" is intentional: when multiple pools are on the same
/// recall plateau, the smaller pool wins on cost.
fn recommend_default(rows: &[SweepRow]) -> usize {
    if rows.is_empty() {
        return RankingWeights::default().rerank_top_k;
    }
    let best_recall_at_5 = rows.iter().map(|r| r.recall_at_5).fold(0.0_f32, f32::max);
    let smallest_p95 = rows.iter().map(|r| r.p95_ms).fold(f32::INFINITY, f32::min);
    let recall_floor = best_recall_at_5 - 0.01;
    let p95_ceiling = smallest_p95 * 1.5;

    rows.iter()
        .filter(|r| r.recall_at_5 >= recall_floor && r.p95_ms <= p95_ceiling)
        .map(|r| r.pool_size)
        .min()
        .unwrap_or(rows[0].pool_size)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf sweep; opt-in via `--ignored rerank_pool_sweep`"]
async fn rerank_pool_sweep() -> Result<()> {
    // Step 1: build the deterministic eval fixture.
    let fixture = ensure_fixture()?;
    let label_to_sha = resolve_labels(&fixture)?;
    let cases = load_golden()?;

    // Isolate the index DB so the sweep doesn't pollute (or get
    // polluted by) a developer's real ~/.ohara state.
    let home = tempfile::tempdir().context("temp OHARA_HOME")?;
    std::env::set_var("OHARA_HOME", home.path());

    // Step 2: index the fixture through the same path the binary uses.
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
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
        workers: None,
    };
    ohara_cli::commands::index::run(index_args)
        .await
        .context("index eval fixture")?;

    // Step 3: build storage + embedder + reranker once. They're identical
    // across pool sizes; only RankingWeights changes.
    let (repo_id, _, _) =
        ohara_cli::commands::resolve_repo_id(&fixture).context("resolve repo id")?;
    let db_path = ohara_cli::commands::index_db_path(&repo_id).context("resolve db path")?;
    let (storage, embedder, reranker) = build_pipeline_components(db_path).await?;

    // Use a fixed "now" so the recency multiplier is stable across runs.
    let now_unix = chrono::DateTime::parse_from_rfc3339("2024-07-01T00:00:00Z")
        .expect("static rfc3339 literal parses")
        .timestamp();

    // Step 4: sweep.
    let mut rows: Vec<SweepRow> = Vec::with_capacity(POOL_SIZES.len());
    for &pool in POOL_SIZES {
        let weights = RankingWeights {
            rerank_top_k: pool,
            ..RankingWeights::default()
        };
        let retriever = Retriever::new(storage.clone(), embedder.clone())
            .with_reranker(reranker.clone())
            .with_weights(weights);

        let row = run_sweep_iteration(&retriever, &repo_id, &cases, &label_to_sha, now_unix, pool)
            .await?;
        eprintln!("{}", serde_json::to_string(&row)?);
        rows.push(row);
    }

    // Step 5: pick the recommended default and print it on stderr.
    let recommended = recommend_default(&rows);
    eprintln!(
        "{}",
        serde_json::json!({
            "recommended_pool_size": recommended,
            "policy": "smallest pool with recall_at_5 within 1% of best AND p95_ms within 1.5x of smallest"
        })
    );
    Ok(())
}

#[cfg(test)]
mod recommend_tests {
    use super::*;

    fn row(pool: usize, r5: f32, p95: f32) -> SweepRow {
        SweepRow {
            pool_size: pool,
            cases: 10,
            recall_at_1: 0.0,
            recall_at_5: r5,
            mrr: 0.0,
            p50_ms: 10.0,
            p95_ms: p95,
        }
    }

    #[test]
    fn picks_smallest_pool_on_recall_plateau() {
        // 50, 100, 150 all sit on the same recall plateau; latency
        // grows. Policy must pick the smallest. Latencies are chosen
        // so the smallest qualifying pool clears the 1.5x p95 ceiling
        // (smallest p95=50, ceiling=75, pool=50 has p95=70).
        let rows = vec![
            row(20, 0.80, 50.0),
            row(50, 0.95, 70.0),
            row(100, 0.95, 130.0),
            row(150, 0.95, 200.0),
        ];
        assert_eq!(recommend_default(&rows), 50);
    }

    #[test]
    fn rejects_pool_when_p95_blows_through_ceiling() {
        // Best recall is at pool=200 but its p95 is 8x the smallest;
        // the 1.5x ceiling rejects it. No pool clears both bars; policy
        // falls back to rows[0].pool_size.
        let rows = vec![
            row(20, 0.80, 50.0),
            row(50, 0.85, 70.0),
            row(200, 1.00, 400.0),
        ];
        assert_eq!(recommend_default(&rows), 20);
    }

    #[test]
    fn handles_empty_rows() {
        let rows: Vec<SweepRow> = vec![];
        assert_eq!(
            recommend_default(&rows),
            RankingWeights::default().rerank_top_k
        );
    }
}
