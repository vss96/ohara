//! `--trace-perf` plumbing — installs a `tracing-subscriber` layer
//! that captures every `ohara::phase` event, accumulates per-phase
//! totals + counts, and prints a compact summary to stderr at
//! process exit.
//!
//! End-user output shape (one line per phase, plus a `sum_of_phases`):
//!
//! ```text
//! [phase] storage_open    8ms    n=1
//! [phase] embed_load   1820ms    n=1
//! [phase] embed_query    12ms    n=1
//! [phase] lane_knn       24ms    n=1   hits=87
//! [phase] sum_of_phases  7042ms
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing::Event;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

#[derive(Default, Clone)]
struct PhaseAcc {
    total_ms: u64,
    calls: u64,
    hits: u64,
}

#[derive(Default, Clone)]
pub struct PerfAccumulator {
    phases: Arc<Mutex<std::collections::BTreeMap<String, PhaseAcc>>>,
}

impl PerfAccumulator {
    /// Print the accumulated per-phase summary to stderr.
    ///
    /// The footer reports `sum_of_phases`, NOT wall-clock — phases run
    /// inside `tokio::join!` (the four lane queries) overlap in real
    /// time, so summing their `elapsed_ms` overestimates wall-clock.
    /// Operators wanting wall-clock should diff the harness output's
    /// `wall_ms` field, not this stderr summary.
    pub fn print_summary_to_stderr(&self) {
        let phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
        let mut total_ms = 0_u64;
        for (name, acc) in phases.iter() {
            total_ms += acc.total_ms;
            let hits_part = if acc.hits > 0 {
                format!("   hits={}", acc.hits)
            } else {
                String::new()
            };
            eprintln!(
                "[phase] {name:<22} {ms:>5}ms   n={n}{hits}",
                name = name,
                ms = acc.total_ms,
                n = acc.calls,
                hits = hits_part,
            );
        }
        eprintln!("[phase] {:<22} {:>5}ms", "sum_of_phases", total_ms);
    }
}

impl<S: tracing::Subscriber> Layer<S> for PerfAccumulator {
    fn on_event(&self, ev: &Event<'_>, _: Context<'_, S>) {
        if ev.metadata().target() != "ohara::phase" {
            return;
        }
        struct V {
            phase: Option<String>,
            elapsed_ms: u64,
            hit_count: u64,
        }
        impl Visit for V {
            fn record_str(&mut self, f: &Field, v: &str) {
                if f.name() == "phase" {
                    self.phase = Some(v.to_string());
                }
            }
            fn record_u64(&mut self, f: &Field, v: u64) {
                match f.name() {
                    "elapsed_ms" => self.elapsed_ms = v,
                    "hit_count" => self.hit_count = v,
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }
        let mut v = V {
            phase: None,
            elapsed_ms: 0,
            hit_count: 0,
        };
        ev.record(&mut v);
        if let Some(name) = v.phase {
            let mut g = self.phases.lock().unwrap_or_else(|e| e.into_inner());
            let entry = g.entry(name).or_default();
            entry.total_ms += v.elapsed_ms;
            entry.calls += 1;
            entry.hits += v.hit_count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    #[test]
    fn aggregator_sums_two_events_for_same_phase() {
        let acc = PerfAccumulator::default();
        let sub = Registry::default().with(acc.clone());
        with_default(sub, || {
            tracing::info!(target: "ohara::phase", phase = "lane_knn", elapsed_ms = 10_u64);
            tracing::info!(target: "ohara::phase", phase = "lane_knn", elapsed_ms = 20_u64, hit_count = 5_u64);
            tracing::info!(target: "ohara::phase", phase = "rrf", elapsed_ms = 1_u64);
        });
        let phases = acc.phases.lock().unwrap();
        let knn = &phases["lane_knn"];
        assert_eq!(knn.calls, 2);
        assert_eq!(knn.total_ms, 30);
        assert_eq!(knn.hits, 5);
        let rrf = &phases["rrf"];
        assert_eq!(rrf.total_ms, 1);
    }
}
