//! Plan 14 Task E.2 — in-process MCP harness. Constructs an
//! `OharaServer` against the medium ripgrep fixture, then drives
//! `find_pattern` and `explain_change` repeatedly. Numbers reflect
//! the **warm** path — cold-load happens once at server boot and is
//! reported as its own phase (`embed_load` / `rerank_load`) the first
//! invocation pulls in.
//!
//! Run:
//! ```sh
//! fixtures/build_medium.sh
//! cargo run --release -p ohara-cli -- index fixtures/medium/repo
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored mcp_query_bench --nocapture
//! ```

use ohara_perf_tests::{current_git_sha, ensure_medium_fixture, workspace_root};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

const ITERATIONS: usize = 10;
const QUERY: &str = "retry with backoff";
// ripgrep's CLI entry point lives at crates/core/main.rs, not src/main.rs.
const EXPLAIN_FILE: &str = "crates/core/main.rs";
const EXPLAIN_LINES: (u32, u32) = (1, 50);

#[derive(Debug, Default, Serialize, Clone)]
struct PhaseStats {
    samples: Vec<u64>,
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
}

#[derive(Debug, Serialize)]
struct McpRunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    boot_ms: u64,
    find_pattern_wall_ms: PhaseStats,
    explain_change_wall_ms: PhaseStats,
    phases: BTreeMap<String, PhaseStats>,
}

#[derive(Default, Clone)]
struct PhaseAcc(Arc<Mutex<BTreeMap<String, PhaseStats>>>);

impl<S: tracing::Subscriber> Layer<S> for PhaseAcc {
    fn on_event(&self, ev: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if ev.metadata().target() != "ohara::phase" {
            return;
        }
        struct V {
            name: Option<String>,
            ms: u64,
        }
        impl Visit for V {
            fn record_str(&mut self, f: &Field, v: &str) {
                if f.name() == "phase" {
                    self.name = Some(v.to_string());
                }
            }
            fn record_u64(&mut self, f: &Field, v: u64) {
                if f.name() == "elapsed_ms" {
                    self.ms = v;
                }
            }
            fn record_debug(&mut self, _f: &Field, _v: &dyn std::fmt::Debug) {}
        }
        let mut v = V { name: None, ms: 0 };
        ev.record(&mut v);
        if let Some(n) = v.name {
            let mut g = self.0.lock().unwrap();
            g.entry(n).or_default().samples.push(v.ms);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf harness — opt in via --ignored"]
async fn mcp_query_bench_emits_run_report() {
    let fixture = ensure_medium_fixture();
    let root = workspace_root();

    use std::sync::OnceLock;
    let acc = PhaseAcc::default();
    let sub = Registry::default().with(acc.clone());
    // Guard against double-init from transitive deps or future
    // sibling tests in this binary. `set_global_default` panics on
    // second call; OnceLock + `is_ok` lets us silently no-op if a
    // dispatcher is already installed (we lose this test's events
    // in that case, which is the right tradeoff vs panicking the
    // whole binary).
    //
    // Caveat: if another test runs first and installs a different
    // subscriber, `acc` won't receive events and `phases` in the
    // report will be empty. That's a non-panicking failure mode —
    // check the report for empty phases if phase numbers look wrong.
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(sub);
    });

    let mut find_wall = PhaseStats::default();
    let mut explain_wall = PhaseStats::default();

    let boot_start = Instant::now();
    let server = ohara_mcp::server::OharaServer::open(&fixture).await.expect(
        "OharaServer::open against medium fixture \
             (run `ohara index fixtures/medium/repo` first)",
    );
    let boot_ms = boot_start.elapsed().as_millis() as u64;
    let service = ohara_mcp::tools::find_pattern::OharaService::new(server);

    for _ in 0..ITERATIONS {
        let req_start = Instant::now();
        let _out = service
            .find_pattern(ohara_mcp::tools::find_pattern::FindPatternInput {
                query: QUERY.to_string(),
                k: 5,
                language: None,
                since: None,
                no_rerank: false,
            })
            .await
            .expect("find_pattern");
        find_wall
            .samples
            .push(req_start.elapsed().as_millis() as u64);
    }

    for _ in 0..ITERATIONS {
        let req_start = Instant::now();
        let _out = service
            .explain_change(ohara_mcp::tools::explain_change::ExplainChangeInput {
                file: EXPLAIN_FILE.to_string(),
                line_start: EXPLAIN_LINES.0,
                line_end: EXPLAIN_LINES.1,
                k: 5,
                include_diff: true,
            })
            .await
            .expect("explain_change");
        explain_wall
            .samples
            .push(req_start.elapsed().as_millis() as u64);
    }

    let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let phases = acc.0.lock().unwrap().clone();
    let report = McpRunReport {
        git_sha: current_git_sha(&root),
        utc,
        iterations: ITERATIONS,
        fixture: fixture.display().to_string(),
        boot_ms,
        find_pattern_wall_ms: find_wall.clone(),
        explain_change_wall_ms: explain_wall.clone(),
        phases,
    };
    let dir = root.join("target/perf/runs");
    std::fs::create_dir_all(&dir).expect("mkdir target/perf/runs");
    let path = dir.join(format!("{}-{}-mcp.json", report.git_sha, report.utc));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&report).expect("serialize report"),
    )
    .expect("write report");

    eprintln!(
        "wrote {} (boot {}ms; find_pattern p50={}ms; explain_change p50={}ms)",
        path.display(),
        boot_ms,
        find_wall.p50(),
        explain_wall.p50(),
    );
    for (name, stats) in &report.phases {
        eprintln!(
            "phase {name:<22} p50={:>5}ms n={}",
            stats.p50(),
            stats.samples.len(),
        );
    }
}
