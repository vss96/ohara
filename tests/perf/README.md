# `tests/perf/` — perf benchmarks + paired quality gates

Plan 6 (v0.6 indexing throughput) lands measurement tooling here.
Everything in this directory is `#[ignore]`'d so the default
`cargo test --workspace` stays fast — opt in with
`--include-ignored` when you actually want to spend the time.

## Layout

| File | Purpose |
|---|---|
| `fetch_questdb.sh` | Idempotent shallow-clone of the QuestDB fixture into `target/perf-fixtures/questdb`. Pins a SHA via `OHARA_QUESTDB_SHA`. |
| `quest_db_baseline.rs` | Plan 6 Task 1.1 — wall-time regression test. Drives `commands::index::run` and asserts a `BASELINE_MS` ceiling. |
| `example_paired.rs` | Plan 6 Task 2.1 skeleton — minimal template for the paired-flag pattern below. Gated behind `#[cfg(any())]` so it doesn't compile until a real flag uses it. |
| `build_context_eval_fixture.sh` | Plan 10 Task 1.2 — builds the deterministic context-engine eval fixture under `target/perf-fixtures/context-engine-eval` (stable author/timestamps so labels resolve to the same SHAs every run). |
| `context_engine_eval.rs` | Plan 10 Task 2.1 — `#[ignore]`'d retrieval-quality runner. Indexes the eval fixture, runs every JSONL case in `fixtures/context_engine_eval/golden.jsonl` through the same `Retriever::find_pattern` path the CLI/MCP use, and emits a one-line JSON metrics summary. |
| `fixtures/context_engine_eval/golden.jsonl` | Plan 10 Task 1.1 — golden cases for the harness. One JSON object per line; see schema below. |

The crate is a workspace member named `ohara-perf-tests` with
`publish = false`. It exists only to host these tests; the `lib.rs`
is intentionally empty.

## Running

```bash
# Default cargo test stays clean — perf benchmarks are skipped.
cargo test --workspace

# Opt in to the full perf suite. Use --release so the numbers reflect
# the shipped profile. Set OHARA_QUESTDB_SHA to bump the fixture pin
# without editing fetch_questdb.sh.
cargo test --workspace --release -- --include-ignored quest_db_baseline
```

The weekly `.github/workflows/perf.yml` job runs the same command
on `macos-14`, posts the breakdown as a workflow notice, and uploads
the log as an artifact. Failures there do **not** gate the release
workflow — perf is informational until Phase 2 lands a real budget.

## The paired-flag quality gate

Any Phase 2 feature flag that flips behavior on the indexing or
retrieval path **must** ship with a paired `#[ignore]`'d e2e test
that exercises both the flag-on and flag-off paths against the same
fixture and asserts retrieval quality is preserved.

The contract from the v0.6 RFC §Constraints (#7):

> A new perf flag may make the flag-on path faster than flag-off.
> It must not change which hunk wins rank-1 on the
> `find_pattern_returns_retry_commit_first` reference query against
> `fixtures/tiny/repo`.

In practice each paired test follows this skeleton:

```rust
#[tokio::test]
#[ignore = "paired e2e — opt in via --include-ignored"]
async fn flag_x_preserves_retry_pattern_rank_1() {
    let fixture = build_or_clone_fixture();

    // Flag OFF: capture the baseline rank-1 hunk id.
    let baseline = run_retry_pattern(&fixture, /* flag = */ false).await;

    // Flag ON: same fixture, same query, flag flipped.
    let candidate = run_retry_pattern(&fixture, /* flag = */ true).await;

    assert_eq!(
        baseline.rank_1_hunk_id, candidate.rank_1_hunk_id,
        "flag flipped rank-1; quality regression"
    );

    // Optional but encouraged: assert the flag is actually faster
    // on the flag-on path so the test doubles as a perf sanity
    // check, not just a quality one.
    assert!(
        candidate.elapsed_ms <= baseline.elapsed_ms,
        "flag-on path slower than flag-off — why ship the flag?"
    );
}
```

`example_paired.rs` is the in-repo skeleton — copy it when adding
the first paired test for a new flag, then wire it into a real
flag's `Args` struct + `Indexer::run` plumbing.

### Why `#[ignore]`?

The reference fixture is too big to vendor and the embedder cold-start
adds a few hundred ms even on a warm cache. Default test runs need
to stay <30s on a developer laptop; perf benchmarks earn their
runtime by being explicitly opted into.

### Why `#[cfg(any())]` on `example_paired.rs`?

`#[cfg(any())]` is a "never compile" gate (an `any()` over zero
predicates is always false). The skeleton is documentation, not a
real test; without the gate it would either fail to compile (no
real flag exists yet) or pass trivially (defeating its purpose as
a template). The `cfg(any())` form is preserved over `cfg(disabled)`
or similar because rustc warns on unrecognised cfg names by default.

## When to add a new perf benchmark

| Trigger | Add a benchmark? |
|---|---|
| New `--flag` that changes the index path | **Yes** — paired e2e per §The paired-flag quality gate |
| New `--flag` that changes only the retrieval / query path | **Yes** if it changes ranking; pair against retry-pattern |
| Internal refactor with no flag surface | No — `quest_db_baseline.rs` already catches wall-time regressions |
| New CLI subcommand (no perf claim) | No |

## Context-engine eval (plan 10)

The retrieval-quality harness is **not** a comprehensive benchmark. It is
a small set of regression tripwires for product-critical queries — "the
queries we ship the demo around". Failures mean a recent change moved a
hand-picked rank-1 hit off the top, which is a reason to look closely,
not necessarily a reason to revert.

### When to run it

Any PR that changes retrieval ranking, chunking, hunk text, symbol
attribution, or query parsing. CONTRIBUTING §14 has the binding rule.

```bash
cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture
```

The `--nocapture` is intentional: the runner emits one structured JSON
line per pass (cases, recall@1, recall@5, mrr, ndcg_lite, p50_ms,
p95_ms, failed ids) plus a per-failed-case dump (top hits, scores,
provenance). Paste that JSON into the PR description.

### `golden.jsonl` schema

One JSON object per line. Fields:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Stable case id used in failure dumps and CI annotations. |
| `query` | string | yes | The natural-language query passed to `find_pattern`. |
| `language` | string \| null | yes | Optional language filter. Null means "all languages" (exercises the no-filter path). |
| `since_unix` | i64 \| null | yes | Optional recency cutoff. Null means "all of history". |
| `expected_commit_labels` | array of string | yes | Ordered by importance. Each label resolves at runtime to a SHA via the fixture's commit-message lookup (so the JSONL doesn't pin hashes). Recall is satisfied if any expected commit appears in the top-K; MRR uses the *first* expected hit. |
| `expected_paths` | array of string | yes | Hint for failure-mode debugging. Not used in scoring today; printed alongside actual paths when a case fails. |
| `notes` | string | yes | Why this case exists. Helps reviewers decide whether a regression is acceptable for the change. |

Add a new case by appending a line to `golden.jsonl` and (if needed) a
new commit to `build_context_eval_fixture.sh`. Don't reuse labels —
each label maps to one SHA.

### Why it's a tripwire, not a benchmark

- The fixture is small enough to index in seconds on a laptop, so it can
  be run by hand on every retrieval-touching PR.
- Initial thresholds (`recall_at_5 == 1.0`, `mrr >= 0.80`, no individual
  query over 2 s) are tight enough to catch obvious regressions but
  loose enough that the harness doesn't become a forcing function for
  meaningless tweaks. Latency is a smoke signal, not the contract.
- Plans 11 and 12 (semantic hunk text, query understanding) are
  evaluated against this harness before/after — that's the harness's
  load-bearing use case.

## Updating `BASELINE_MS`

When Plan 6 Phase 2 ships an optimization, follow this loop:

1. Run `quest_db_baseline.rs` against `main` — record the number.
2. Land the optimization on a branch.
3. Re-run the baseline — compare.
4. Update `BASELINE_MS` to the new run's median wall-time.
5. Update `docs/perf/v0.6-baseline.md` with the before/after table.

The 10% `REGRESSION_TOLERANCE` already in the test absorbs CI noise;
don't pad the new constant beyond that or future regressions hide
behind it.
