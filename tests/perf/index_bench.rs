//! Plan 15 Task A.2 — indexing-path memory + wall-time harness.
//!
//! Runs `ohara index` against a *fresh copy* of the medium ripgrep
//! fixture (so each iteration is a cold full index, not an
//! incremental no-op) and writes peak-RSS + wall-time numbers to
//! `target/perf/runs/<git_sha>-<utc>-index.json`.
//!
//! Run:
//! ```sh
//! fixtures/build_medium.sh
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored index_bench --nocapture
//! ```
//!
//! For the authoritative *child*-process peak RSS, run:
//!     /usr/bin/time -l target/release/ohara index <fixture> --no-progress
//! and grep "maximum resident set size". The harness's reported
//! `peak_rss_bytes` is the *parent* (test) process's peak — useful
//! as a relative comparison across runs but not the absolute number.
use ohara_perf_tests::{current_git_sha, ensure_medium_fixture, peak_rss_bytes, workspace_root};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const ITERATIONS: usize = 3;

#[derive(Debug, Serialize)]
struct IterReport {
    wall_ms: u64,
    peak_rss_bytes: u64,
    new_commits: u64,
    new_hunks: u64,
}

#[derive(Debug, Serialize)]
struct RunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    iters: Vec<IterReport>,
}

/// Parse the CLI's one-line summary:
///   `indexed: <N> new commits, <M> hunks, <K> HEAD symbols`
fn parse_report_line(stdout: &str) -> (u64, u64) {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("indexed: ") {
            let mut commits = 0u64;
            let mut hunks = 0u64;
            for part in rest.split(", ") {
                let mut it = part.split_whitespace();
                let n: u64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
                let kind = it.next().unwrap_or("");
                match kind {
                    "new" => commits = n,
                    "hunks" => hunks = n,
                    _ => {}
                }
            }
            return (commits, hunks);
        }
    }
    (0, 0)
}

fn copy_fixture(src: &std::path::Path) -> PathBuf {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dst = tmp.path().join("repo");
    let status = Command::new("cp")
        .arg("-R")
        .arg(src)
        .arg(&dst)
        .status()
        .expect("cp -R");
    assert!(status.success(), "cp -R failed");
    // Leak the tempdir so the path stays valid for the index run.
    // The OS cleans up when the test process exits.
    std::mem::forget(tmp);
    dst
}

fn write_report(report: &RunReport) -> PathBuf {
    let root = workspace_root();
    let dir = root.join("target/perf/runs");
    std::fs::create_dir_all(&dir).expect("mkdir target/perf/runs");
    let path = dir.join(format!("{}-{}-index.json", report.git_sha, report.utc));
    let json = serde_json::to_string_pretty(report).expect("serialize");
    std::fs::write(&path, json).expect("write report");
    path
}

/// Placeholder: getting the child's *exact* peak RSS portably from
/// inside this harness is brittle (`RUSAGE_CHILDREN` aggregates across
/// every child the test process has reaped, and we spawn multiple).
/// The harness reports the *parent*'s peak, which moves with the
/// child indirectly via fs cache pressure but does NOT include the
/// child's anonymous pages. Operators who need the authoritative
/// child number run `/usr/bin/time -l target/release/ohara index ...`
/// directly — see the file-level docs.
fn child_peak_rss(_out: &std::process::Output) -> u64 {
    peak_rss_bytes().unwrap_or(0)
}

#[test]
#[ignore = "perf harness — opt in via --ignored"]
fn index_bench_emits_run_report() {
    let fixture = ensure_medium_fixture();
    let root = workspace_root();
    let bin = root.join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    let mut iters = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let work = copy_fixture(&fixture);
        let ohara_home = tempfile::tempdir().expect("tempdir");
        let start = Instant::now();
        let out = Command::new(&bin)
            .env("OHARA_HOME", ohara_home.path())
            .arg("index")
            .arg(&work)
            .arg("--no-progress")
            .arg("--embed-provider")
            .arg("cpu")
            .output()
            .expect("spawn ohara index");
        let wall_ms = start.elapsed().as_millis() as u64;
        if !out.status.success() {
            panic!(
                "ohara index failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (commits, hunks) = parse_report_line(&stdout);
        let child_peak = child_peak_rss(&out);
        iters.push(IterReport {
            wall_ms,
            peak_rss_bytes: child_peak,
            new_commits: commits,
            new_hunks: hunks,
        });
    }

    let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let report = RunReport {
        git_sha: current_git_sha(&root),
        utc,
        iterations: ITERATIONS,
        fixture: fixture.display().to_string(),
        iters,
    };
    let path = write_report(&report);
    eprintln!("wrote {}", path.display());
    for (i, it) in report.iters.iter().enumerate() {
        eprintln!(
            "iter {i}: wall={}ms peak_rss={} MiB commits={} hunks={}",
            it.wall_ms,
            it.peak_rss_bytes / (1024 * 1024),
            it.new_commits,
            it.new_hunks,
        );
    }
}
