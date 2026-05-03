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

/// Shared test fixture for capturing `ohara::phase` events emitted by
/// `timed_phase` under libtest parallelism.
///
/// A per-test thread-local subscriber is racy: a parallel test's noop
/// dispatcher can win the `DefaultCallsite::register()` race and cache
/// `Interest::never()` for `timed_phase`'s callsite before ours installs,
/// making events invisible.
///
/// Fix: install one global subscriber for the whole test binary via
/// `set_global_default` + `OnceLock`. Callsites are then registered with
/// `Interest::always()` from the start. Each caller of
/// `acquire_phase_collector` gets an exclusive `Arc<Mutex<BTreeSet>>`
/// (serialised behind a global mutex) that the shared layer writes into.
#[cfg(test)]
pub(crate) mod test_phase_capture {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex, OnceLock};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;

    pub(crate) type PhaseSink = Mutex<Option<Arc<Mutex<BTreeSet<String>>>>>;

    /// Returns the global phase-event sink slot.
    pub(crate) fn phase_sink() -> &'static PhaseSink {
        static SINK: OnceLock<PhaseSink> = OnceLock::new();
        SINK.get_or_init(|| Mutex::new(None))
    }

    struct PhaseLayer;

    struct PhaseVisitor<'a>(&'a mut Option<String>);
    impl Visit for PhaseVisitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "phase" {
                *self.0 = Some(value.to_owned());
            }
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber> Layer<S> for PhaseLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "ohara::phase" {
                return;
            }
            let mut phase: Option<String> = None;
            event.record(&mut PhaseVisitor(&mut phase));
            if let Some(p) = phase {
                if let Some(sink) = phase_sink().lock().unwrap().as_ref() {
                    sink.lock().unwrap().insert(p);
                }
            }
        }
    }

    pub(crate) struct PhaseGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for PhaseGuard {
        fn drop(&mut self) {
            *phase_sink().lock().unwrap() = None;
            // self.0 (the serialisation lock guard) drops here.
        }
    }

    /// Acquires exclusive access to the phase-event capture slot.
    ///
    /// Returns `(seen, guard)`. While `guard` is live the global `PhaseLayer`
    /// writes `ohara::phase` event names into `seen`. Dropping `guard` clears
    /// the slot and releases the serialisation lock.
    pub(crate) fn acquire_phase_collector() -> (Arc<Mutex<BTreeSet<String>>>, PhaseGuard) {
        // Install the global subscriber exactly once per process.
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::Registry::default().with(PhaseLayer),
            )
            .expect("global tracing subscriber set once");
        });

        // Serialise so only one test writes to the sink at a time.
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock_guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let seen: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        *phase_sink().lock().unwrap() = Some(Arc::clone(&seen));
        (seen, PhaseGuard(lock_guard))
    }
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

    struct PhaseEventFull {
        phase: Option<String>,
        has_elapsed_ms: bool,
        hit_count: Option<u64>,
    }

    struct PhaseVisitorFull<'a>(&'a mut PhaseEventFull);

    impl Visit for PhaseVisitorFull<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "phase" {
                self.0.phase = Some(value.to_owned());
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "elapsed_ms" {
                self.0.has_elapsed_ms = true;
            }
            if field.name() == "hit_count" {
                self.0.hit_count = Some(value);
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    struct CaptureLayerFull {
        captured: Arc<Mutex<Vec<PhaseEventFull>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayerFull {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "ohara::phase" {
                return;
            }
            let mut pe = PhaseEventFull {
                phase: None,
                has_elapsed_ms: false,
                hit_count: None,
            };
            event.record(&mut PhaseVisitorFull(&mut pe));
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

    #[test]
    fn timed_phase_with_count_emits_phase_elapsed_ms_and_hit_count() {
        use super::timed_phase_with_count;

        let captured: Arc<Mutex<Vec<PhaseEventFull>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayerFull {
            captured: Arc::clone(&captured),
        };
        let subscriber = tracing_subscriber::Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(async {
                let result =
                    timed_phase_with_count("rerank", async { (vec!["a", "b", "c"], 3) }).await;
                assert_eq!(result, vec!["a", "b", "c"]);
            });
        });

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1, "expected exactly one phase event");
        let ev = &events[0];
        assert_eq!(ev.phase.as_deref(), Some("rerank"), "phase name mismatch");
        assert!(ev.has_elapsed_ms, "elapsed_ms field was not recorded");
        assert_eq!(ev.hit_count, Some(3), "hit_count field mismatch");
    }
}
