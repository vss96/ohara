//! Plan 14 Task E.1 — CLI cold-path perf harness.
//!
//! Runs `ohara query --trace-perf` N times against the medium ripgrep
//! fixture and writes per-phase histograms to
//! `target/perf/runs/<git_sha>-<utc>-cli-query.json`. Designed to be
//! operator-run (`#[ignore]`'d) — the spec calls for harness numbers
//! in PR descriptions, not CI gates.
//!
//! Run:
//! ```sh
//! fixtures/build_medium.sh
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored cli_query_bench --nocapture
//! ```

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const ITERATIONS: usize = 5;
const QUERY: &str = "retry with backoff";

#[derive(Debug, Default, Serialize, Clone)]
struct PhaseStats {
    samples: Vec<u64>, // ms
}

impl PhaseStats {
    fn p50(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        s[s.len() / 2]
    }
    fn min(&self) -> u64 {
        *self.samples.iter().min().unwrap_or(&0)
    }
    fn max(&self) -> u64 {
        *self.samples.iter().max().unwrap_or(&0)
    }
}

#[derive(Debug, Serialize)]
struct RunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    query: String,
    wall_ms: PhaseStats,
    phases: BTreeMap<String, PhaseStats>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root resolves")
        .to_path_buf()
}

fn ensure_medium_fixture() -> PathBuf {
    let root = workspace_root();
    let script = root.join("fixtures/build_medium.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("invoke build_medium.sh");
    assert!(status.success(), "build_medium.sh failed");
    let dest = root.join("fixtures/medium/repo");
    assert!(dest.join(".git").is_dir(), "medium fixture not present");
    dest
}

fn current_git_sha(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Parse one `[phase] <name>     <N>ms ...` line from `--trace-perf`
/// stderr. Returns `(name, ms)` or `None` when the line is the trailing
/// `total` summary or any unrelated stderr noise.
fn parse_phase_line(line: &str) -> Option<(String, u64)> {
    let l = line.trim_start();
    let rest = l.strip_prefix("[phase] ")?;
    let mut it = rest.split_whitespace();
    let name = it.next()?.to_string();
    if name == "total" {
        return None;
    }
    let ms_token = it.next()?;
    let ms_str = ms_token.strip_suffix("ms")?;
    let ms: u64 = ms_str.parse().ok()?;
    Some((name, ms))
}

fn write_report(report: &RunReport) -> PathBuf {
    let root = workspace_root();
    let dir = root.join("target/perf/runs");
    std::fs::create_dir_all(&dir).expect("mkdir target/perf/runs");
    let path = dir.join(format!("{}-{}-cli-query.json", report.git_sha, report.utc));
    let json = serde_json::to_string_pretty(report).expect("serialize report");
    std::fs::write(&path, json).expect("write report");
    path
}

#[test]
#[ignore = "perf harness — opt in via --ignored"]
fn cli_query_bench_emits_run_report() {
    let fixture = ensure_medium_fixture();
    let root = workspace_root();
    let bin = root.join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    let mut wall = PhaseStats::default();
    let mut phases: BTreeMap<String, PhaseStats> = BTreeMap::new();

    for i in 0..ITERATIONS {
        let start = Instant::now();
        let out = Command::new(&bin)
            .arg("--trace-perf")
            .arg("query")
            .arg(&fixture)
            .arg("--query")
            .arg(QUERY)
            .arg("--no-rerank") // exclude rerank cold-load on the cold-CLI path
            .arg("--embed-provider")
            .arg("cpu") // pin to CPU ONNX so harness works on any build variant
            .output()
            .expect("spawn ohara");
        let elapsed = start.elapsed().as_millis() as u64;
        wall.samples.push(elapsed);
        if !out.status.success() {
            panic!(
                "iter {i}: ohara query failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            if let Some((name, ms)) = parse_phase_line(line) {
                phases.entry(name).or_default().samples.push(ms);
            }
        }
    }

    let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let report = RunReport {
        git_sha: current_git_sha(&root),
        utc,
        iterations: ITERATIONS,
        fixture: fixture.display().to_string(),
        query: QUERY.to_string(),
        wall_ms: wall,
        phases,
    };
    let path = write_report(&report);
    eprintln!("wrote {}", path.display());
    eprintln!(
        "wall p50={}ms min={}ms max={}ms",
        report.wall_ms.p50(),
        report.wall_ms.min(),
        report.wall_ms.max()
    );
    for (name, stats) in &report.phases {
        eprintln!(
            "phase {name:<22} p50={:>5}ms min={:>5}ms max={:>5}ms n={}",
            stats.p50(),
            stats.min(),
            stats.max(),
            stats.samples.len()
        );
    }
}
