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
//! The harness wraps the `ohara index` invocation with
//! `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) to
//! capture the child-process peak RSS directly from the OS. The
//! result is parsed from the `time` output's stderr and recorded
//! in `peak_rss_bytes`. If `/usr/bin/time` is unavailable on the
//! host, the harness falls back to the parent-process peak (same
//! caveat as before: useful as a relative comparison only).
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

/// Parse the CLI's summary header (PR #66):
///   `indexed in <duration> — <N> commit(s), <M> hunk(s), <K> HEAD symbol(s)`
fn parse_report_line(stdout: &str) -> (u64, u64) {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("indexed in ") {
            // After "indexed in <duration>": " — N commits, M hunks, K HEAD symbols"
            let Some(idx) = rest.find(" — ") else {
                continue;
            };
            let counts = &rest[idx + " — ".len()..];
            let mut commits = 0u64;
            let mut hunks = 0u64;
            for part in counts.split(", ") {
                let mut it = part.split_whitespace();
                let n: u64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
                let kind = it.next().unwrap_or("");
                match kind {
                    "commit" | "commits" => commits = n,
                    "hunk" | "hunks" => hunks = n,
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

fn time_command() -> &'static str {
    "/usr/bin/time"
}

#[cfg(target_os = "macos")]
fn time_flag() -> &'static str {
    "-l"
}
#[cfg(target_os = "linux")]
fn time_flag() -> &'static str {
    "-v"
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn time_flag() -> &'static str {
    "-l" // best-effort; unsupported platforms fall back to harness peak
}

/// Parse `/usr/bin/time` stderr for the child's peak RSS in bytes.
///
/// macOS BSD `time -l` line:
///     "        12345678  maximum resident set size"
/// (number is bytes; line is whitespace-prefixed).
///
/// Linux GNU `time -v` line:
///     "\tMaximum resident set size (kbytes): 12345"
/// (number is kilobytes; we multiply by 1024).
///
/// Returns `None` when the line isn't found (e.g. /usr/bin/time missing on
/// the host, unsupported platform), so the caller can fall back gracefully.
fn parse_time_rss(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        // macOS: "<bytes>  maximum resident set size"
        if let Some(rest) = trimmed.strip_suffix("maximum resident set size") {
            return rest.trim().parse::<u64>().ok();
        }
        // Linux: "Maximum resident set size (kbytes): <kb>"
        if let Some(rest) = trimmed.strip_prefix("Maximum resident set size (kbytes): ") {
            return rest.trim().parse::<u64>().ok().map(|kb| kb * 1024);
        }
    }
    None
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
        let out = Command::new(time_command())
            .arg(time_flag())
            .arg(&bin)
            .env("OHARA_HOME", ohara_home.path())
            .arg("index")
            .arg(&work)
            .arg("--no-progress")
            .arg("--embed-provider")
            .arg("cpu")
            .output()
            .expect("spawn /usr/bin/time + ohara index");
        let wall_ms = start.elapsed().as_millis() as u64;
        if !out.status.success() {
            panic!(
                "ohara index failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let (commits, hunks) = parse_report_line(&stdout);
        let child_peak = parse_time_rss(&stderr).unwrap_or_else(|| {
            eprintln!("warning: /usr/bin/time RSS line not found; falling back to parent peak");
            peak_rss_bytes().unwrap_or(0)
        });
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

#[cfg(test)]
mod tests {
    use super::{parse_report_line, parse_time_rss};

    #[test]
    fn parses_post_pr66_summary_header() {
        let stdout = "\
indexed in 47.3s — 1670 commits, 5951 hunks, 36976 HEAD symbols

  embed    38.1s  ████████████████████████████████   80%
  storage   4.2s  ████                                9%
";
        assert_eq!(parse_report_line(stdout), (1670, 5951));
    }

    #[test]
    fn parses_singular_units() {
        // Edge case: 1 commit, 1 hunk — singular nouns must still parse.
        let stdout = "indexed in 1.0s — 1 commit, 1 hunk, 1 HEAD symbol\n";
        assert_eq!(parse_report_line(stdout), (1, 1));
    }

    #[test]
    fn returns_zeros_when_header_missing() {
        let stdout = "ohara: indexing complete\nsome unrelated noise\n";
        assert_eq!(parse_report_line(stdout), (0, 0));
    }

    #[test]
    fn parses_macos_time_l_format() {
        let stderr = "\
indexed in 1.2s — 100 commits, 500 hunks, 0 HEAD symbols
        12345678  maximum resident set size
             1024  page reclaims
";
        assert_eq!(parse_time_rss(stderr), Some(12_345_678));
    }

    #[test]
    fn parses_linux_time_v_format() {
        let stderr = "\
indexed in 1.2s — 100 commits, 500 hunks, 0 HEAD symbols
\tMaximum resident set size (kbytes): 1500
\tAverage resident set size (kbytes): 0
";
        assert_eq!(parse_time_rss(stderr), Some(1_500 * 1024));
    }

    #[test]
    fn returns_none_when_no_rss_line_present() {
        let stderr = "ohara: indexing complete\nsome unrelated stderr noise\n";
        assert_eq!(parse_time_rss(stderr), None);
    }
}
