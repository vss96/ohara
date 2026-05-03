//! Plan 14 Task E.3 — diff two perf runs.
//!
//! `OHARA_PERF_DIFF_BEFORE` and `OHARA_PERF_DIFF_AFTER` env vars point
//! at two JSON reports produced by `cli_query_bench` /
//! `mcp_query_bench`. The test prints a per-phase delta to stderr.
//!
//! Run:
//! ```sh
//! cargo test -p ohara-perf-tests --release -- --ignored perf_diff --nocapture
//! ```
//!
//! With env vars set, e.g.:
//! ```sh
//! OHARA_PERF_DIFF_BEFORE=target/perf/runs/AAA-cli-query.json \
//! OHARA_PERF_DIFF_AFTER=target/perf/runs/BBB-cli-query.json \
//! cargo test -p ohara-perf-tests --release -- --ignored perf_diff --nocapture
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct PhaseStats {
    samples: Vec<u64>,
}
impl PhaseStats {
    fn p50(&self) -> i64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        s[s.len() / 2] as i64
    }
}

#[derive(Debug, Deserialize)]
struct RunReportLite {
    #[serde(default)]
    wall_ms: Option<PhaseStats>,
    #[serde(default)]
    find_pattern_wall_ms: Option<PhaseStats>,
    #[serde(default)]
    explain_change_wall_ms: Option<PhaseStats>,
    phases: BTreeMap<String, PhaseStats>,
}

fn read(path: &Path) -> RunReportLite {
    let s = std::fs::read_to_string(path).expect("read perf report");
    serde_json::from_str(&s).expect("parse perf report")
}

#[test]
#[ignore = "perf-diff utility — opt in via --ignored"]
fn perf_diff_prints_per_phase_delta() {
    let before_path = std::env::var("OHARA_PERF_DIFF_BEFORE")
        .expect("set OHARA_PERF_DIFF_BEFORE=<path-to-before.json>");
    let after_path = std::env::var("OHARA_PERF_DIFF_AFTER")
        .expect("set OHARA_PERF_DIFF_AFTER=<path-to-after.json>");
    let before = read(Path::new(&before_path));
    let after = read(Path::new(&after_path));

    eprintln!("phase                  before    after    delta");
    eprintln!("---------------------- -------- -------- --------");
    let names: std::collections::BTreeSet<&String> =
        before.phases.keys().chain(after.phases.keys()).collect();
    for name in names {
        let b = before.phases.get(name).map(|s| s.p50()).unwrap_or(0);
        let a = after.phases.get(name).map(|s| s.p50()).unwrap_or(0);
        let delta = a - b;
        eprintln!("{name:<22} {b:>6}ms {a:>6}ms {delta:>+6}ms");
    }

    if let (Some(b), Some(a)) = (
        before.wall_ms.as_ref().map(|s| s.p50()),
        after.wall_ms.as_ref().map(|s| s.p50()),
    ) {
        let delta = a - b;
        eprintln!("wall (cli)             {b:>6}ms {a:>6}ms {delta:>+6}ms");
    }
    if let (Some(b), Some(a)) = (
        before.find_pattern_wall_ms.as_ref().map(|s| s.p50()),
        after.find_pattern_wall_ms.as_ref().map(|s| s.p50()),
    ) {
        let delta = a - b;
        eprintln!("find_pattern (mcp)     {b:>6}ms {a:>6}ms {delta:>+6}ms");
    }
    if let (Some(b), Some(a)) = (
        before.explain_change_wall_ms.as_ref().map(|s| s.p50()),
        after.explain_change_wall_ms.as_ref().map(|s| s.p50()),
    ) {
        let delta = a - b;
        eprintln!("explain_change (mcp)   {b:>6}ms {a:>6}ms {delta:>+6}ms");
    }
}
