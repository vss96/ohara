//! Plan-27 perf harness: run `ohara index` against a fixture three
//! times (--embed-cache off|semantic|diff) and print embed wall-time +
//! cache row counts side-by-side. Operator-run; not in CI.
//!
//! Run:
//!
//! ```sh
//! fixtures/build_tiny.sh            # or build_medium.sh for a larger fixture
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored embed_cache_sweep --nocapture
//! ```
//!
//! The test requires `OHARA_FIXTURE` to be set to a git repo path, or
//! falls back to `fixtures/tiny/repo` relative to the workspace root.

use ohara_perf_tests::workspace_root;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const MODES: &[&str] = &["off", "semantic", "diff"];

fn resolve_fixture() -> PathBuf {
    if let Ok(p) = std::env::var("OHARA_FIXTURE") {
        return PathBuf::from(p);
    }
    workspace_root().join("fixtures/tiny/repo")
}

#[test]
#[ignore = "perf harness — opt in via --ignored embed_cache_sweep --nocapture"]
fn embed_cache_sweep() {
    let fixture = resolve_fixture();
    assert!(
        fixture.join(".git").is_dir(),
        "fixture repo not found at {} — run fixtures/build_tiny.sh or set OHARA_FIXTURE",
        fixture.display()
    );

    let bin = workspace_root().join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    for mode in MODES {
        let ohara_home = tempfile::tempdir().expect("tempdir for OHARA_HOME");
        eprintln!("\n=== mode={mode} ===");
        let start = Instant::now();
        let status = Command::new(&bin)
            .env("OHARA_HOME", ohara_home.path())
            .args([
                "index",
                "--rebuild",
                "--yes",
                "--embed-provider",
                "cpu",
                "--embed-cache",
                mode,
            ])
            .arg(&fixture)
            .status()
            .expect("spawn ohara index");
        let elapsed = start.elapsed();
        eprintln!("mode={mode} status={status} elapsed={elapsed:?}");

        if *mode != "off" {
            let st = Command::new(&bin)
                .env("OHARA_HOME", ohara_home.path())
                .arg("status")
                .arg(&fixture)
                .output()
                .expect("spawn ohara status");
            let stdout = String::from_utf8_lossy(&st.stdout);
            for line in stdout.lines() {
                if line.starts_with("embed_cache:") {
                    eprintln!("  {line}");
                }
            }
        }
    }
}
