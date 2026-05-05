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
use std::path::PathBuf;
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
    // Skeleton — body filled in by Tasks A.3 through A.6.
    let _cases = load_golden()?;
    let _ = (POOL_SIZES, std::any::type_name::<SweepRow>());
    let _ = std::any::type_name::<Retriever>();
    let _ = std::any::type_name::<RankingWeights>();
    let _ = std::any::type_name::<PatternQuery>();
    let _: Option<Arc<()>> = None;
    let _ = Instant::now();
    Ok(())
}
