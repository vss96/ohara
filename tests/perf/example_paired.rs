//! Plan 6 Task 2.1 — paired-flag quality-gate skeleton.
//!
//! Copy this file (and its test) when adding the first paired e2e
//! for a Phase 2 perf flag. Until a real flag exists, the file is
//! gated behind `#[cfg(any())]` (the canonical "never compile"
//! cfg) so it documents the pattern without breaking the build.
//!
//! See `tests/perf/README.md` §"The paired-flag quality gate" for
//! the contract this skeleton implements.
//!
//! When you copy this file:
//!   1. Drop the `#![cfg(any())]` line below.
//!   2. Replace `FlagXArgs` with the real `commands::index::Args`
//!      construction for your flag (e.g. `embed_provider`).
//!   3. Wire the actual `find_pattern` retrieval call instead of
//!      the placeholder `run_retry_pattern` stub.
//!   4. Update `tests/perf/Cargo.toml` if you need new dev-deps.

#![cfg(any())]

use std::path::PathBuf;
use std::time::Instant;

/// Placeholder for the (Phase-2-defined) flag under test. Replace
/// with the real flag-enum from `commands::index::Args` before the
/// skeleton ships behind a real flag.
#[derive(Debug, Clone, Copy)]
struct FlagXArgs {
    /// Flag-on / flag-off knob. The paired test asserts identical
    /// rank-1 across both values.
    flag_x_enabled: bool,
}

/// Result of one retry-pattern lookup. Captured for both halves of
/// the paired run so the test compares apples to apples.
struct RetryPatternResult {
    rank_1_hunk_id: i64,
    elapsed_ms: u128,
}

async fn run_retry_pattern(_fixture: &PathBuf, _args: FlagXArgs) -> RetryPatternResult {
    // TODO when copying:
    //   - Build / open the fixture (re-use fixtures/tiny/repo or a
    //     shallow-clone helper from this directory).
    //   - Run `commands::index::run` with `args` plumbed in.
    //   - Run `commands::query::run` (or call the retriever
    //     directly) for the retry-pattern probe and capture the
    //     top hit's hunk id.
    //   - Time the full path with Instant::now() so the test
    //     doubles as a perf sanity check.
    let start = Instant::now();
    let elapsed_ms = start.elapsed().as_millis();
    RetryPatternResult {
        rank_1_hunk_id: 0,
        elapsed_ms,
    }
}

#[tokio::test]
#[ignore = "paired e2e skeleton — opt in via --include-ignored once a real flag uses this"]
async fn flag_x_preserves_retry_pattern_rank_1() {
    // Plan 6 RFC §Constraints (#7) — a new perf flag may make the
    // flag-on path faster than flag-off, but it must NOT change
    // which hunk wins rank-1 on the retry-pattern probe.
    let fixture = PathBuf::from("fixtures/tiny/repo");

    let baseline = run_retry_pattern(
        &fixture,
        FlagXArgs {
            flag_x_enabled: false,
        },
    )
    .await;
    let candidate = run_retry_pattern(
        &fixture,
        FlagXArgs {
            flag_x_enabled: true,
        },
    )
    .await;

    assert_eq!(
        baseline.rank_1_hunk_id, candidate.rank_1_hunk_id,
        "flag flipped rank-1 on retry-pattern — quality regression"
    );

    // Optional but encouraged: the perf claim itself.
    assert!(
        candidate.elapsed_ms <= baseline.elapsed_ms,
        "flag-on path ({} ms) slower than flag-off ({} ms) — why ship the flag?",
        candidate.elapsed_ms,
        baseline.elapsed_ms,
    );
}
