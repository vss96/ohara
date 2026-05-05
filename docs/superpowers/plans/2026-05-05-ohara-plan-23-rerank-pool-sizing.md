# ohara plan-23 — Rerank pool sizing + perf-frontier benchmark

> **Status:** complete

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each green
> implementation.

**Goal:** measure ohara's recall/latency frontier as a function of
`RankingWeights::rerank_top_k` against the existing context-engine
eval fixture, and decide (data-driven) whether to widen the default
pool from 50 toward Anthropic's published 150-pool baseline. Ship
either (a) a new default + per-profile overrides backed by the sweep,
or (b) keep the current 50 with a documented "we measured and 50 is on
the knee" note.

**Architecture:** new `tests/perf/rerank_pool_sweep.rs` runner reuses
the deterministic eval fixture built by
`tests/perf/build_context_eval_fixture.sh` and the same
`Retriever::find_pattern_with_profile` path the CLI / MCP exercise. For
each pool size in `[20, 50, 100, 150, 200]` the runner re-constructs a
`Retriever` with that `rerank_top_k`, replays every `golden.jsonl`
case, and emits one JSON-lines row per (pool_size, metric) tuple to
stderr. A small post-processor in the same file picks the smallest
pool whose `recall_at_5` is within 1% of the best observed
`recall_at_5` and whose `p95_ms` is within 1.5x of the smallest
observed `p95_ms`; that becomes the recommended default.

**Tech Stack:** Rust 2021, existing `tests/perf` harness conventions,
no new crates.

**Spec:** none — extends the plan-10 eval methodology
(`docs/superpowers/plans/2026-05-02-ohara-plan-10-context-engine-evals.md`)
with a parameter sweep. Anthropic's "Contextual Retrieval" post is the
external reference point — they used 150→20.

**Scope check:** plan-23 only touches `tests/perf` (new sweep runner),
plus a one-line default change in `crates/ohara-core/src/retriever.rs`
*if* the data supports it. No new public API, no storage / embed /
binary changes. The default-change task (B.2) is conditional on the
sweep result.

---

## Phase A — Sweep runner

### Task A.1 — New `rerank_pool_sweep.rs` runner skeleton

**Files:**
- Create: `tests/perf/rerank_pool_sweep.rs`
- Modify: `tests/perf/Cargo.toml` (register the new `[[test]]` target)

- [ ] **Step 1: Inspect `tests/perf/Cargo.toml` to understand the existing target style**

Run: `cat tests/perf/Cargo.toml`

Expected: a list of `[[test]]` blocks, each with `name = "..."` and
`path = "..."`. Note the format so the new entry matches.

- [ ] **Step 2: Add the new `[[test]]` block in `tests/perf/Cargo.toml`**

Append (preserving the alphabetical / chronological ordering used by
the file — slot it after `context_engine_eval`):

```toml
[[test]]
name = "rerank_pool_sweep"
path = "rerank_pool_sweep.rs"
harness = true
```

- [ ] **Step 3: Create `tests/perf/rerank_pool_sweep.rs` with the runner skeleton**

Write the following file. The body deliberately mirrors
`context_engine_eval.rs`'s structure (fixture build, golden-case
load, retriever construction, replay loop) so a future reader who
already understands the eval harness gets the sweep for free. The
`#[ignore]` attribute matches the existing perf-runner convention so
`cargo test --workspace` stays fast.

```rust
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

use anyhow::{Context, Result};
use ohara_core::query::PatternQuery;
use ohara_core::retriever::RankingWeights;
use ohara_core::Retriever;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

const POOL_SIZES: &[usize] = &[20, 50, 100, 150, 200];
const FIXTURE_DIR: &str = "fixtures/context_engine_eval/repo";
const GOLDEN_PATH: &str = "fixtures/context_engine_eval/golden.jsonl";

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

#[tokio::test]
#[ignore = "perf sweep; opt-in via `--ignored rerank_pool_sweep`"]
async fn rerank_pool_sweep() -> Result<()> {
    // Step 1: build the fixture exactly the way context_engine_eval does.
    build_fixture().context("building eval fixture")?;

    // Step 2: load every golden case (reuse the same JSONL parser
    // approach as context_engine_eval — duplicate the small struct +
    // loader rather than introducing a shared helper crate).
    let cases = load_golden(GOLDEN_PATH)?;

    // Step 3: build storage + embedder + reranker once. They're
    // identical across pool sizes; only RankingWeights changes.
    let (storage, embedder, reranker) = build_pipeline_components().await?;
    let repo_id = open_fixture_repo(&storage).await?;
    let now_unix = chrono::Utc::now().timestamp();

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

        let row = run_sweep_iteration(&retriever, &repo_id, &cases, now_unix, pool).await?;
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

fn build_fixture() -> Result<()> {
    let status = Command::new("bash")
        .arg("tests/perf/build_context_eval_fixture.sh")
        .status()
        .context("running build_context_eval_fixture.sh")?;
    if !status.success() {
        anyhow::bail!("fixture build script returned {status}");
    }
    Ok(())
}

// --- Helpers below mirror context_engine_eval.rs verbatim where the
//     responsibility is identical (golden loading, pipeline construction).
//     Duplication is intentional per CONTRIBUTING.md: a perf runner is a
//     standalone operator tool and should be readable end-to-end without
//     chasing helpers across files.

// load_golden, build_pipeline_components, open_fixture_repo,
// run_sweep_iteration, recommend_default — see Tasks A.2 through A.6.
```

- [ ] **Step 4: Run the new test with `--no-run` to confirm the file compiles**

Run: `cargo test -p ohara-perf-tests --test rerank_pool_sweep --no-run`

Expected: compiles successfully but emits unused-import warnings for
the helpers we haven't implemented yet. Tasks A.2–A.6 fill them in.

- [ ] **Step 5: Commit the skeleton**

```bash
git add tests/perf/Cargo.toml tests/perf/rerank_pool_sweep.rs
git commit -m "perf(plan-23): add rerank-pool sweep runner skeleton"
```

### Task A.2 — Implement `load_golden` (golden-case loader)

**Files:**
- Modify: `tests/perf/rerank_pool_sweep.rs`

- [ ] **Step 1: Open `tests/perf/context_engine_eval.rs` and read the existing `GoldenCase` struct + JSONL loader**

Run: `grep -n "struct GoldenCase\|fn load_golden\|fn read_jsonl" tests/perf/context_engine_eval.rs`

Expected: the eval already has a struct + loader. Read those blocks
verbatim — the sweep runner reuses the schema field-for-field.

- [ ] **Step 2: Add a `GoldenCase` struct and `load_golden` to the sweep runner**

Append to `tests/perf/rerank_pool_sweep.rs`:

```rust
#[derive(Debug, serde::Deserialize)]
struct GoldenCase {
    id: String,
    query: String,
    language: Option<String>,
    since_unix: Option<i64>,
    expected_commit_labels: Vec<String>,
    #[allow(dead_code)]
    expected_paths: Vec<String>,
    #[allow(dead_code)]
    expected_profile: Option<String>,
}

fn load_golden(path: &str) -> Result<Vec<GoldenCase>> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).with_context(|| format!("open {path}"))?;
    let mut out = Vec::new();
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line.with_context(|| format!("read {path} line {}", i + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let case: GoldenCase = serde_json::from_str(trimmed)
            .with_context(|| format!("parse {path} line {} ({trimmed})", i + 1))?;
        out.push(case);
    }
    Ok(out)
}
```

- [ ] **Step 3: Run a smoke check**

Run: `cargo test -p ohara-perf-tests --test rerank_pool_sweep --no-run`

Expected: compiles. (We can't actually invoke the test until Tasks A.3–A.6 land.)

- [ ] **Step 4: Commit**

```bash
git add tests/perf/rerank_pool_sweep.rs
git commit -m "perf(plan-23): implement load_golden for sweep runner"
```

### Task A.3 — Implement `build_pipeline_components` and `open_fixture_repo`

**Files:**
- Modify: `tests/perf/rerank_pool_sweep.rs`

- [ ] **Step 1: Read the equivalent block in `tests/perf/context_engine_eval.rs`**

Run: `grep -n "FastEmbedProvider\|FastEmbedReranker\|SqliteStorage\|open_repo\|fn build_pipeline\|fn open_fixture_repo" tests/perf/context_engine_eval.rs`

Expected: the eval constructs `FastEmbedProvider`, `FastEmbedReranker`,
and a `SqliteStorage` then calls `open_repo`. Reuse the same
construction.

- [ ] **Step 2: Add `build_pipeline_components` and `open_fixture_repo`**

Append to `tests/perf/rerank_pool_sweep.rs` (use the **exact**
provider / storage / repo-id construction the eval uses; copy it
verbatim — no abstraction):

```rust
async fn build_pipeline_components() -> Result<(
    Arc<dyn ohara_core::Storage>,
    Arc<dyn ohara_core::EmbeddingProvider>,
    Arc<dyn ohara_core::embed::RerankProvider>,
)> {
    use ohara_embed::fastembed::{FastEmbedProvider, FastEmbedReranker, EmbedProvider};
    use ohara_storage::SqliteStorage;

    // Match context_engine_eval: SqliteStorage at fixed path,
    // CPU-only fastembed (no CoreML / CUDA in CI).
    let db_path: PathBuf = "fixtures/context_engine_eval/index.db".into();
    let storage: Arc<dyn ohara_core::Storage> =
        Arc::new(SqliteStorage::open(&db_path).context("open SqliteStorage")?);
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> =
        Arc::new(FastEmbedProvider::with_provider(EmbedProvider::Cpu)?);
    let reranker: Arc<dyn ohara_core::embed::RerankProvider> =
        Arc::new(FastEmbedReranker::with_provider(EmbedProvider::Cpu)?);
    Ok((storage, embedder, reranker))
}

async fn open_fixture_repo(
    storage: &Arc<dyn ohara_core::Storage>,
) -> Result<ohara_core::types::RepoId> {
    let repo_id = ohara_core::types::RepoId::from_parts("eval", FIXTURE_DIR);
    storage
        .open_repo(&repo_id, "fake-origin", "main")
        .await
        .context("open_repo for fixture")?;
    Ok(repo_id)
}
```

> **Note:** if the actual constructor signatures in `ohara-embed` /
> `ohara-storage` differ from the calls above — they evolve over time —
> mirror what `context_engine_eval.rs` does **today** rather than what
> this plan was written against. The task is "construct the same
> pipeline the eval constructs"; the exact symbol names are illustrative.

- [ ] **Step 3: Compile-check**

Run: `cargo test -p ohara-perf-tests --test rerank_pool_sweep --no-run`

Expected: compiles. If a constructor name changed since this plan was
written, fix it to match the eval and re-run.

- [ ] **Step 4: Commit**

```bash
git add tests/perf/rerank_pool_sweep.rs
git commit -m "perf(plan-23): wire pipeline-component construction in sweep runner"
```

### Task A.4 — Implement `run_sweep_iteration`

**Files:**
- Modify: `tests/perf/rerank_pool_sweep.rs`

- [ ] **Step 1: Add the iteration body**

Append to `tests/perf/rerank_pool_sweep.rs`:

```rust
async fn run_sweep_iteration(
    retriever: &Retriever,
    repo_id: &ohara_core::types::RepoId,
    cases: &[GoldenCase],
    now_unix: i64,
    pool: usize,
) -> Result<SweepRow> {
    let mut latencies_ms: Vec<f32> = Vec::with_capacity(cases.len());
    let mut hits_at_1: usize = 0;
    let mut hits_at_5: usize = 0;
    let mut rr_sum: f32 = 0.0;

    for case in cases {
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

        // Resolve expected_commit_labels → SHAs by looking up the
        // commit-message-prefix in the returned set. This mirrors the
        // eval runner's resolution scheme: the golden file pins
        // labels (stable across fixture rebuilds), not SHAs (which
        // would churn).
        let expected_shas: Vec<String> = case
            .expected_commit_labels
            .iter()
            .filter_map(|label| {
                hits.iter()
                    .find(|h| h.commit_message.starts_with(label))
                    .map(|h| h.commit_sha.clone())
            })
            .collect();

        let any_in_top1 = hits
            .first()
            .map(|h| expected_shas.iter().any(|sha| sha == &h.commit_sha))
            .unwrap_or(false);
        if any_in_top1 {
            hits_at_1 += 1;
        }
        let any_in_top5 = hits
            .iter()
            .take(5)
            .any(|h| expected_shas.iter().any(|sha| sha == &h.commit_sha));
        if any_in_top5 {
            hits_at_5 += 1;
        }
        if let Some(rank) = hits
            .iter()
            .take(5)
            .position(|h| expected_shas.iter().any(|sha| sha == &h.commit_sha))
        {
            rr_sum += 1.0 / ((rank + 1) as f32);
        }
    }

    latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = latencies_ms.len().max(1);
    let p50 = latencies_ms[(n * 50 / 100).min(n - 1)];
    let p95 = latencies_ms[(n * 95 / 100).min(n - 1)];
    Ok(SweepRow {
        pool_size: pool,
        cases: cases.len(),
        recall_at_1: hits_at_1 as f32 / cases.len() as f32,
        recall_at_5: hits_at_5 as f32 / cases.len() as f32,
        mrr: rr_sum / cases.len() as f32,
        p50_ms: p50,
        p95_ms: p95,
    })
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo test -p ohara-perf-tests --test rerank_pool_sweep --no-run`

Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add tests/perf/rerank_pool_sweep.rs
git commit -m "perf(plan-23): implement per-pool-size sweep iteration"
```

### Task A.5 — Implement `recommend_default`

**Files:**
- Modify: `tests/perf/rerank_pool_sweep.rs`

- [ ] **Step 1: Add the recommendation policy**

Append to `tests/perf/rerank_pool_sweep.rs`:

```rust
/// Pick the smallest pool whose `recall_at_5` is within 1% of the best
/// observed AND whose `p95_ms` is within 1.5x of the smallest observed.
/// "Smallest" is intentional: when multiple pools are on the same
/// recall plateau, the smaller pool wins on cost.
fn recommend_default(rows: &[SweepRow]) -> usize {
    if rows.is_empty() {
        return RankingWeights::default().rerank_top_k;
    }
    let best_recall_at_5 = rows
        .iter()
        .map(|r| r.recall_at_5)
        .fold(0.0_f32, f32::max);
    let smallest_p95 = rows
        .iter()
        .map(|r| r.p95_ms)
        .fold(f32::INFINITY, f32::min);
    let recall_floor = best_recall_at_5 - 0.01;
    let p95_ceiling = smallest_p95 * 1.5;

    rows.iter()
        .filter(|r| r.recall_at_5 >= recall_floor && r.p95_ms <= p95_ceiling)
        .map(|r| r.pool_size)
        .min()
        .unwrap_or(rows[0].pool_size)
}
```

- [ ] **Step 2: Add a unit test for `recommend_default`**

Append at the bottom of the same file (before the final closing brace
if any):

```rust
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
        // grows. Policy must pick the smallest.
        let rows = vec![
            row(20, 0.80, 50.0),
            row(50, 0.95, 80.0),
            row(100, 0.95, 130.0),
            row(150, 0.95, 200.0),
        ];
        assert_eq!(recommend_default(&rows), 50);
    }

    #[test]
    fn rejects_pool_when_p95_blows_through_ceiling() {
        // Best recall is at pool=200 but its p95 is 6x the smallest;
        // the 1.5x ceiling rejects it. Policy must fall back to the
        // smallest pool that's still within both bounds.
        let rows = vec![
            row(20, 0.80, 50.0),
            row(50, 0.85, 70.0),
            row(200, 1.00, 400.0),
        ];
        // Best recall = 1.00, floor = 0.99 — only pool=200 qualifies on
        // recall, but its p95 (400) > 1.5 * 50 = 75, so it's rejected.
        // No pool clears both bars; policy falls back to rows[0].pool_size.
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
```

- [ ] **Step 3: Run the unit test**

Run: `cargo test -p ohara-perf-tests --test rerank_pool_sweep recommend_tests`

Expected: 3 tests pass. (These tests do NOT require the fixture or the
`#[ignore]`'d outer test, so they run in the default `cargo test`.)

- [ ] **Step 4: Commit**

```bash
git add tests/perf/rerank_pool_sweep.rs
git commit -m "perf(plan-23): add recommend_default policy + unit tests"
```

### Task A.6 — End-to-end smoke run

- [ ] **Step 1: Run the sweep against the eval fixture**

Run:
```sh
cargo test -p ohara-perf-tests -- --ignored rerank_pool_sweep --nocapture
```

Expected: stderr contains 5 JSONL rows (one per pool size) followed by
the `recommended_pool_size` line. The whole sweep should complete in
under ~10 minutes on CPU (cross-encoder dominates; 5 pool sizes × ~30
golden cases × ≤200 candidates each).

- [ ] **Step 2: Capture the output to a baseline file**

Save the sweep's JSONL block (5 rows + the recommendation row) to:

`tests/perf/baselines/rerank_pool_sweep.jsonl`

Create the directory if it doesn't exist:

```bash
mkdir -p tests/perf/baselines
# paste the 6 lines from stderr into:
$EDITOR tests/perf/baselines/rerank_pool_sweep.jsonl
```

- [ ] **Step 3: Commit the baseline**

```bash
git add tests/perf/baselines/rerank_pool_sweep.jsonl
git commit -m "perf(plan-23): record initial rerank-pool sweep baseline"
```

---

## Phase B — Conditional default change

### Task B.1 — Decision point

- [ ] **Step 1: Read the recommendation row from the baseline**

Open `tests/perf/baselines/rerank_pool_sweep.jsonl` and identify the
`recommended_pool_size` value.

- [ ] **Step 2: Decide**

| Recommended pool | Action |
|---|---|
| `50` | Skip Task B.2 — current default is on the knee. Update plan-23 status to `complete` with a note. |
| any other value | Proceed to Task B.2. |

### Task B.2 — Update default `RankingWeights::rerank_top_k` (CONDITIONAL)

Skip this task if Task B.1 said "current default is on the knee."

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs:37-46`

- [ ] **Step 1: Add a failing assertion test that pins the new default**

Insert into the existing `#[cfg(test)] mod tests` block of
`crates/ohara-core/src/retriever.rs`:

```rust
#[test]
fn ranking_weights_default_rerank_pool_matches_plan_23_baseline() {
    // plan-23 sweep concluded the recommended pool is `<NEW_VALUE>`.
    // Pin it so a future drift triggers a CI failure and forces
    // re-running the sweep.
    assert_eq!(RankingWeights::default().rerank_top_k, <NEW_VALUE>);
}
```

Replace `<NEW_VALUE>` with the literal recommended size from Task B.1
(e.g. `100`).

- [ ] **Step 2: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib ranking_weights_default_rerank_pool_matches_plan_23_baseline`

Expected: fails (current default is 50).

- [ ] **Step 3: Update the default**

In `crates/ohara-core/src/retriever.rs`, change the `Default` impl
for `RankingWeights`:

```rust
impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            recency_weight: 0.05,
            recency_half_life_days: 90.0,
            rerank_top_k: <NEW_VALUE>,  // plan-23 baseline
            lane_top_k: 100,
        }
    }
}
```

- [ ] **Step 4: Run the test and confirm it now passes**

Run: `cargo test -p ohara-core --lib ranking_weights_default_rerank_pool_matches_plan_23_baseline`

Expected: PASS.

- [ ] **Step 5: Run the full retriever test suite**

Run: `cargo test -p ohara-core --lib retriever::`

Expected: all tests pass. The pool-size change is purely additive (a
larger pool keeps strictly more candidates), so existing fakes that
return ≤5 candidates are unaffected.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "perf(retriever): bump default rerank_top_k to <NEW_VALUE> (plan-23 sweep)"
```

---

## Phase C — Final gate

### Task C.1 — Workspace gate + plan status

- [ ] **Step 1: Full workspace gate**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Run: `cargo test --workspace`

Expected: all three pass.

- [ ] **Step 2: Update plan status**

Edit
`docs/superpowers/plans/2026-05-05-ohara-plan-23-rerank-pool-sizing.md`
and change `> **Status:** draft` to `> **Status:** complete`.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/plans/2026-05-05-ohara-plan-23-rerank-pool-sizing.md
git commit -m "docs(plan-23): mark complete"
```
