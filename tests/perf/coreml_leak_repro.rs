//! Plan 7 Task 1.1 — minimal reproduction harness for the CoreML
//! memory leak documented in
//! `docs/superpowers/specs/2026-05-02-ohara-v0.6.1-coreml-leak-rfc.md`.
//!
//! Hardware-bound and intentionally `#[ignore]`'d. Run manually on
//! Apple Silicon with the `coreml` feature built. The harness:
//!
//!   * Constructs a `FastEmbedProvider` with the requested execution
//!     provider (default: CoreML).
//!   * Calls `embed_batch` in a tight loop with synthetic strings.
//!   * Optionally rebuilds the embedder every `K` iterations to probe
//!     Task 2.1 (rebuild-cadence).
//!   * Prints its PID up front so an operator can sample memory from
//!     a second terminal — see the §Sampling block below.
//!
//! ## Build
//!
//! ```sh
//! cargo build --release \
//!     --features ohara-embed/coreml \
//!     -p ohara-perf-tests --tests
//! ```
//!
//! ## Run
//!
//! ```sh
//! cargo test --release \
//!     --features ohara-embed/coreml \
//!     -p ohara-perf-tests \
//!     --test coreml_leak_repro -- --include-ignored --nocapture
//! ```
//!
//! ## Knobs (all optional environment variables)
//!
//! | Var               | Default | Meaning                                       |
//! | ----------------- | ------- | --------------------------------------------- |
//! | `LEAK_PROVIDER`   | coreml  | `cpu` or `coreml`                             |
//! | `LEAK_BATCHES`    | 5000    | total `embed_batch` calls                     |
//! | `LEAK_BATCH_SIZE` | 16      | strings per batch                             |
//! | `LEAK_TEXT_LEN`   | 800     | bytes per string (~200 BPE tokens)            |
//! | `LEAK_REBUILD_K`  | unset   | rebuild embedder every K iters (probe 2.1)    |
//! | `LEAK_SLEEP_MS`   | 0       | pause between batches (probe 1.4 wallclock)   |
//! | `LEAK_REPORT_EVERY` | 100   | progress line cadence                         |
//!
//! ## Sampling (run in a second terminal)
//!
//! ```sh
//! PID=<from harness stderr>
//! while kill -0 $PID 2>/dev/null; do
//!     printf '[%s] ' "$(date +%T)"
//!     footprint -p $PID 2>/dev/null \
//!         | awk -F'[: ]+' '/phys_footprint/ {print "phys_footprint=" $2 " " $3; exit}'
//!     sleep 5
//! done | tee /tmp/coreml_leak.footprint.log
//! ```
//!
//! For the region-level breakdown referenced in plan-7 Task 1.2 step 2:
//! `vmmap --summary $PID > /tmp/coreml_leak.vmmap.$(date +%H%M%S).txt`.

use std::env;
use std::time::{Duration, Instant};

use ohara_core::EmbeddingProvider;
use ohara_embed::{EmbedProvider, FastEmbedProvider};

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_optional<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

fn provider_from_env() -> EmbedProvider {
    let raw = env::var("LEAK_PROVIDER").unwrap_or_else(|_| "coreml".into());
    match raw.as_str() {
        "cpu" => EmbedProvider::Cpu,
        "coreml" => EmbedProvider::CoreMl,
        other => panic!("LEAK_PROVIDER must be 'cpu' or 'coreml', got {other:?}"),
    }
}

/// Build a synthetic input string of approximately `len` bytes.
/// Salting the prefix prevents the model from caching identical inputs
/// across iterations.
fn make_text(len: usize, salt: usize) -> String {
    let mut buf = format!("[salt={salt}] ");
    while buf.len() < len {
        buf.push_str("the quick brown fox jumps over the lazy dog. ");
    }
    buf.truncate(len);
    buf
}

#[ignore = "hardware-bound: requires CoreML build, run manually for plan-7 diagnosis"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coreml_leak_repro() {
    let provider = provider_from_env();
    let batches: usize = env_parse("LEAK_BATCHES", 5000);
    let batch_size: usize = env_parse("LEAK_BATCH_SIZE", 16);
    let text_len: usize = env_parse("LEAK_TEXT_LEN", 800);
    let rebuild_k: Option<usize> = env_optional("LEAK_REBUILD_K");
    let sleep_ms: u64 = env_parse("LEAK_SLEEP_MS", 0);
    let report_every: usize = env_parse("LEAK_REPORT_EVERY", 100);

    eprintln!(
        "coreml_leak_repro: pid={} provider={:?} batches={} batch_size={} text_len={} rebuild_k={:?} sleep_ms={}",
        std::process::id(),
        provider,
        batches,
        batch_size,
        text_len,
        rebuild_k,
        sleep_ms,
    );

    let mut embedder =
        FastEmbedProvider::with_provider(provider).expect("test harness: failed to init embedder");
    let started = Instant::now();
    let mut window_started = Instant::now();

    for iter in 0..batches {
        if let Some(k) = rebuild_k {
            if iter > 0 && iter % k == 0 {
                eprintln!("[iter {iter}] rebuilding embedder (probe 2.1, K={k})");
                embedder = FastEmbedProvider::with_provider(provider)
                    .expect("test harness: failed to rebuild embedder");
            }
        }

        let texts: Vec<String> = (0..batch_size)
            .map(|j| make_text(text_len, iter * batch_size + j))
            .collect();

        embedder
            .embed_batch(&texts)
            .await
            .expect("test harness: embed_batch failed mid-run");

        if (iter + 1) % report_every == 0 {
            let elapsed = window_started.elapsed();
            let per_batch = elapsed.as_millis() as f64 / report_every as f64;
            eprintln!(
                "[iter {}/{}] window {:.0}ms/batch ({:.1} batches/s) total {:?}",
                iter + 1,
                batches,
                per_batch,
                report_every as f64 / elapsed.as_secs_f64(),
                started.elapsed(),
            );
            window_started = Instant::now();
        }

        if sleep_ms > 0 {
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
    }

    eprintln!(
        "coreml_leak_repro: done batches={} total_elapsed={:?}",
        batches,
        started.elapsed()
    );
}
