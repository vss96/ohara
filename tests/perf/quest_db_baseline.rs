//! Plan 6 Task 1.1 — `#[ignore]`'d perf benchmark against the
//! pinned QuestDB fixture. Drives the same code path that ships in
//! the `ohara` binary (`commands::index::run`) so the wall-time
//! measured here matches what users see end-to-end.
//!
//! Run with:
//! ```sh
//! cargo test --workspace --release -- --include-ignored quest_db_baseline
//! ```
//!
//! The fixture is fetched on demand via `tests/perf/fetch_questdb.sh`
//! (idempotent) so the test is self-contained — no manual setup.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

/// Wall-time budget in ms. **Placeholder** — Plan 6 Task 0.5 captures
/// the real number from a manual run and updates this constant in a
/// follow-up commit. Until then, 60_000 is a permissive ceiling that
/// catches catastrophic regressions (1+ minute) without flaking on
/// the actual ~6h baseline.
///
/// TODO: replace after Task 0.5 runs (see docs/perf/v0.6-baseline.md
/// §"CI threshold update").
const BASELINE_MS: u128 = 60_000;

/// Multiplier applied to BASELINE_MS to allow for noise on shared CI
/// runners. 1.10 = "fail on a 10% regression". Pulled out as a
/// constant so the threshold is auditable in PR diffs.
const REGRESSION_TOLERANCE: f64 = 1.10;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at tests/perf; pop two segments to
    // get the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root resolves")
        .to_path_buf()
}

fn ensure_fixture() -> PathBuf {
    let root = workspace_root();
    let script = root.join("tests/perf/fetch_questdb.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("invoke fetch_questdb.sh");
    assert!(
        status.success(),
        "fetch_questdb.sh failed; check network + git availability"
    );
    let dest = root.join("target/perf-fixtures/questdb");
    assert!(
        dest.join(".git").is_dir(),
        "fixture missing after fetch_questdb.sh: {}",
        dest.display()
    );
    dest
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf benchmark — opt in via --include-ignored, runs against an 800MB fixture"]
async fn quest_db_baseline_under_threshold() {
    let fixture = ensure_fixture();

    // Isolate the index DB so the benchmark doesn't pollute (or get
    // polluted by) a developer's real ~/.ohara state.
    let home = tempfile::tempdir().expect("temp OHARA_HOME");
    std::env::set_var("OHARA_HOME", home.path());

    let args = ohara_cli::commands::index::Args {
        path: fixture,
        incremental: false,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: true,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
    };

    let start = Instant::now();
    let report = ohara_cli::commands::index::run(args)
        .await
        .expect("index run");
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_millis();

    // Emit the JSON breakdown so the GitHub Actions runner can pick
    // it up as a workflow annotation. Stderr keeps it out of stdout
    // assertion machinery.
    let pt = &report.phase_timings;
    eprintln!(
        "perf::quest_db_baseline elapsed_ms={elapsed_ms} \
         commit_walk_ms={} diff_extract_ms={} embed_ms={} \
         storage_write_ms={} head_symbols_ms={} \
         total_diff_bytes={} total_added_lines={} \
         new_commits={} new_hunks={} head_symbols={}",
        pt.commit_walk_ms,
        pt.diff_extract_ms,
        pt.embed_ms,
        pt.storage_write_ms,
        pt.head_symbols_ms,
        pt.total_diff_bytes,
        pt.total_added_lines,
        report.new_commits,
        report.new_hunks,
        report.head_symbols,
    );

    let ceiling = (BASELINE_MS as f64 * REGRESSION_TOLERANCE) as u128;
    assert!(
        elapsed_ms < ceiling,
        "quest_db baseline regressed: {elapsed_ms}ms >= {ceiling}ms \
         (BASELINE_MS={BASELINE_MS}, tolerance={REGRESSION_TOLERANCE})"
    );

    // Sanity check: the run actually indexed something. A 0-commit
    // pass would silently pass the threshold.
    assert!(
        report.new_commits > 0,
        "quest_db baseline produced no commits — fixture broken?"
    );
}
