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
