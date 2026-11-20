//! Plan 33 Task 15 — worktree visibility filter latency guard.
//!
//! Builds the plan-10 eval fixture, clones it into a temp dir, adds one
//! linked worktree carrying 20 extra commits, and indexes both checkouts
//! into one shared store. The worktree's tip is not reachable from the
//! main clone's HEAD, so every `find_pattern` from the main clone takes
//! the ancestry-filtered path. The harness then times 200 calls with the
//! filter active and 200 with it bypassed (interleaved, so drift hits
//! both arms equally) and prints one line:
//!
//! ```text
//! perf::visibility_filter {"p50_ms_on":..,"p95_ms_on":..,"p50_ms_off":..,"p95_ms_off":..,"added_p95_ms":..}
//! ```
//!
//! Asserts `added_p95_ms < 5.0` (spec §13 phase 1a).
//!
//! Queries run with `no_rerank: true`. The filter sits before fusion and
//! the rerank pool cut, so the two arms hand the cross-encoder different
//! hunks (with the filter on, the 20 short worktree hunks are replaced by
//! the main clone's longer ones). Rerank time then tracks hunk length, not
//! filter cost, and would swamp the number this guard exists to measure.
//!
//! The bypassed arm uses `RetrievalEngine::set_visibility_bypass`, which
//! only exists under `ohara-engine`'s `perf-hooks` feature.
//!
//! Run:
//! ```sh
//! cargo test -p ohara-perf-tests -- --ignored visibility_filter --nocapture
//! ```

use anyhow::{Context, Result};
use ohara_core::query::PatternQuery;
use ohara_engine::RetrievalEngine;
use ohara_perf_tests::workspace_root;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

const CALLS_PER_ARM: usize = 200;
const WORKTREE_COMMITS: usize = 20;
const MAX_ADDED_P95_MS: f64 = 5.0;
const QUERY: &str = "retry with exponential backoff";

#[derive(Debug, Serialize)]
struct Summary {
    calls_per_arm: usize,
    p50_ms_on: f64,
    p95_ms_on: f64,
    p50_ms_off: f64,
    p95_ms_off: f64,
    added_p95_ms: f64,
    filtered_queries: u64,
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "ohara-fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@ohara.test")
        .env("GIT_COMMITTER_NAME", "ohara-fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@ohara.test")
        .output()
        .with_context(|| format!("git {args:?}"))?;
    anyhow::ensure!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn ensure_eval_fixture() -> Result<PathBuf> {
    let root = workspace_root();
    let status = Command::new("bash")
        .arg(root.join("tests/perf/build_context_eval_fixture.sh"))
        .status()
        .context("invoke build_context_eval_fixture.sh")?;
    anyhow::ensure!(status.success(), "fixture builder failed");
    Ok(root.join("target/perf-fixtures/context-engine-eval"))
}

/// Clone the shared eval fixture (so the eval's own checkout is never
/// mutated) and add a linked worktree with `WORKTREE_COMMITS` commits.
fn build_repo_with_worktree(scratch: &Path) -> Result<(PathBuf, PathBuf)> {
    let fixture = ensure_eval_fixture()?;
    let main = scratch.join("main");
    let status = Command::new("git")
        .args(["clone", "-q"])
        .arg(&fixture)
        .arg(&main)
        .status()
        .context("git clone fixture")?;
    anyhow::ensure!(status.success(), "git clone fixture failed");
    let wt = scratch.join("wt");
    let wt_str = wt.to_string_lossy().to_string();
    git(&main, &["worktree", "add", "-q", "-b", "perf-wt", &wt_str])?;
    for i in 0..WORKTREE_COMMITS {
        let body = format!(
            "use std::time::Duration;\n\n\
             /// Worktree-only retry variant {i}.\n\
             pub fn retry_with_backoff_{i}(attempts: u32) -> Duration {{\n    \
             // exponential backoff with jitter, variant {i}\n    \
             Duration::from_millis(100u64 << attempts.min({i} + 1))\n}}\n"
        );
        std::fs::write(wt.join(format!("src/retry_{i}.rs")), body)
            .with_context(|| format!("write retry_{i}.rs"))?;
        git(&wt, &["add", "-A"])?;
        let msg = format!("retry: worktree backoff variant {i}");
        git(&wt, &["commit", "-q", "-m", &msg])?;
    }
    Ok((main, wt))
}

fn index_args(path: PathBuf) -> ohara_cli::commands::index::Args {
    ohara_cli::commands::index::Args {
        path,
        interactive: false,
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
    }
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let idx = (((pct / 100.0) * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

async fn timed_call(engine: &RetrievalEngine, repo: &Path, q: &PatternQuery) -> Result<f64> {
    let start = Instant::now();
    engine
        .find_pattern(repo, q.clone())
        .await
        .context("find_pattern")?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf guard — opt in via --ignored; downloads embedder + reranker on first run"]
async fn visibility_filter_adds_under_5ms_p95() -> Result<()> {
    let home = tempfile::tempdir().context("temp OHARA_HOME")?;
    std::env::set_var("OHARA_HOME", home.path());
    let scratch = tempfile::tempdir().context("scratch dir")?;
    let (main, wt) = build_repo_with_worktree(scratch.path())?;

    ohara_cli::commands::index::run(index_args(main.clone()), true)
        .await
        .context("index main clone")?;
    ohara_cli::commands::index::run(index_args(wt.clone()), true)
        .await
        .context("index linked worktree")?;

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
    let engine = RetrievalEngine::new(embedder, reranker);
    let q = PatternQuery {
        query: QUERY.into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };

    // Warm-up: open the handle, load the reranker, fill the embed cache.
    timed_call(&engine, &main, &q).await?;
    anyhow::ensure!(
        engine.ancestry_filtered_queries() == 1,
        "fixture must force the filtered path from the main clone"
    );

    let mut on = Vec::with_capacity(CALLS_PER_ARM);
    let mut off = Vec::with_capacity(CALLS_PER_ARM);
    for _ in 0..CALLS_PER_ARM {
        engine.set_visibility_bypass(false);
        on.push(timed_call(&engine, &main, &q).await?);
        engine.set_visibility_bypass(true);
        off.push(timed_call(&engine, &main, &q).await?);
    }
    engine.set_visibility_bypass(false);
    on.sort_by(f64::total_cmp);
    off.sort_by(f64::total_cmp);

    let p95_on = percentile(&on, 95.0);
    let p95_off = percentile(&off, 95.0);
    let summary = Summary {
        calls_per_arm: CALLS_PER_ARM,
        p50_ms_on: percentile(&on, 50.0),
        p95_ms_on: p95_on,
        p50_ms_off: percentile(&off, 50.0),
        p95_ms_off: p95_off,
        added_p95_ms: p95_on - p95_off,
        filtered_queries: engine.ancestry_filtered_queries(),
    };
    println!(
        "perf::visibility_filter {}",
        serde_json::to_string(&summary).context("serialize summary")?
    );
    assert!(
        summary.added_p95_ms < MAX_ADDED_P95_MS,
        "visibility filter added {:.3} ms at p95 (limit {MAX_ADDED_P95_MS} ms)",
        summary.added_p95_ms
    );
    Ok(())
}
