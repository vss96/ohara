//! Plan-28 perf harness: run `ohara index` against a fixture with
//! varying worker counts (--workers 1|2|4|8) and print wall-time per
//! pass. Operator-run; not in CI.
//!
//! Run:
//!
//! ```sh
//! fixtures/build_tiny.sh            # or build_medium.sh for a larger fixture
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored parallel_indexer_sweep --nocapture
//! ```
//!
//! The test requires `OHARA_PERF_REPO` to be set to a git repo path, or
//! falls back to `fixtures/tiny/repo` relative to the workspace root.

use ohara_perf_tests::workspace_root;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const WORKER_COUNTS: &[&str] = &["1", "2", "4", "8"];

fn resolve_fixture() -> PathBuf {
    if let Ok(p) = std::env::var("OHARA_PERF_REPO") {
        return PathBuf::from(p);
    }
    workspace_root().join("fixtures/tiny/repo")
}

#[test]
#[ignore = "perf harness — opt in via --ignored parallel_indexer_sweep --nocapture"]
fn parallel_indexer_sweep() {
    let fixture = resolve_fixture();
    assert!(
        fixture.join(".git").is_dir(),
        "fixture repo not found at {} — run fixtures/build_tiny.sh or set OHARA_PERF_REPO",
        fixture.display()
    );

    let bin = workspace_root().join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    for workers in WORKER_COUNTS {
        let ohara_home = tempfile::tempdir().expect("tempdir for OHARA_HOME");
        eprintln!("\n=== workers={workers} ===");
        let start = Instant::now();
        let status = Command::new(&bin)
            .env("OHARA_HOME", ohara_home.path())
            .args([
                "index",
                "--rebuild",
                "--yes",
                "--embed-provider",
                "cpu",
                "--workers",
                workers,
            ])
            .arg(&fixture)
            .status()
            .expect("spawn ohara index");
        let elapsed = start.elapsed();
        eprintln!("workers={workers} status={status} elapsed={elapsed:?}");
    }
}
