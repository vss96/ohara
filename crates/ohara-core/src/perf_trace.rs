//! Phase-level tracing helper. Every async section we want timed in
//! the perf harness is wrapped with [`timed_phase`]; the helper emits
//! exactly one `tracing::info!` event on target `ohara::phase` with
//! `phase = <name>` and `elapsed_ms = <u64>`.
//!
//! The harness installs a `tracing-subscriber` layer that filters on
//! `target == "ohara::phase"` and aggregates by phase name. End users
//! never see these events unless they pass `--trace-perf` or set
//! `RUST_LOG=ohara::phase=info`.

use std::future::Future;
use std::time::Instant;

/// Run `fut` and emit a phase event capturing its elapsed time.
///
/// `name` is `'static` so the subscriber can use it as a stable
/// aggregation key; phase names are part of the perf harness contract
/// (see the spec's tracing schema) and adding a new one is a real
/// product change, not an ad-hoc literal.
pub async fn timed_phase<T, F: Future<Output = T>>(name: &'static str, fut: F) -> T {
    let start = Instant::now();
    let out = fut.await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::info!(target: "ohara::phase", phase = name, elapsed_ms);
    out
}

/// Like [`timed_phase`] but additionally records `hit_count` for lane
/// queries / rerank stages where row count is part of the harness
/// signal.
pub async fn timed_phase_with_count<T, F: Future<Output = (T, usize)>>(
    name: &'static str,
    fut: F,
) -> T {
    let start = Instant::now();
    let (out, count) = fut.await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "ohara::phase",
        phase = name,
        elapsed_ms,
        hit_count = count as u64,
    );
    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;

    struct PhaseEvent {
        phase: Option<String>,
        has_elapsed_ms: bool,
    }

    struct PhaseVisitor<'a>(&'a mut PhaseEvent);

    impl Visit for PhaseVisitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "phase" {
                self.0.phase = Some(value.to_owned());
            }
        }

        fn record_u64(&mut self, field: &Field, _value: u64) {
            if field.name() == "elapsed_ms" {
                self.0.has_elapsed_ms = true;
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    struct CaptureLayer {
        captured: Arc<Mutex<Vec<PhaseEvent>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "ohara::phase" {
                return;
            }
            let mut pe = PhaseEvent {
                phase: None,
                has_elapsed_ms: false,
            };
            event.record(&mut PhaseVisitor(&mut pe));
            self.captured.lock().unwrap().push(pe);
        }
    }

    #[test]
    fn timed_phase_emits_one_event_with_phase_and_elapsed_ms() {
        use super::timed_phase;

        let captured: Arc<Mutex<Vec<PhaseEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            captured: Arc::clone(&captured),
        };
        let subscriber = tracing_subscriber::Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(async {
                let result = timed_phase("lane_knn", async { 42_u32 }).await;
                assert_eq!(result, 42_u32);
            });
        });

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1, "expected exactly one phase event");
        let ev = &events[0];
        assert_eq!(ev.phase.as_deref(), Some("lane_knn"), "phase name mismatch");
        assert!(ev.has_elapsed_ms, "elapsed_ms field was not recorded");
    }
}
