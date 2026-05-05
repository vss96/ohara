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
use serde::Serialize;
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf sweep; opt-in via `--ignored rerank_pool_sweep`"]
async fn rerank_pool_sweep() -> Result<()> {
    // Skeleton — body filled in by Tasks A.2 through A.6.
    let _ = (POOL_SIZES, std::any::type_name::<SweepRow>());
    let _ = std::any::type_name::<Retriever>();
    let _ = std::any::type_name::<RankingWeights>();
    let _ = std::any::type_name::<PatternQuery>();
    let _: Option<Arc<()>> = None;
    let _ = Instant::now();
    let _: Result<()> = Ok::<(), std::io::Error>(()).context("noop");
    Ok(())
}
