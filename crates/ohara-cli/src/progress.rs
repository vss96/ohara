//! `tracing-indicatif`-backed `ProgressSink` for the `ohara index` CLI command.
//!
//! Drives a progress bar via a `tracing` span rather than owning a bare
//! `indicatif::ProgressBar`. The bar is materialized by the global
//! `IndicatifLayer` (registered in `main::init_tracing`), which:
//!
//! * pins the bar to the bottom of the terminal,
//! * routes `tracing` log lines through `MultiProgress::suspend(...)` so
//!   they print *above* the bar instead of scrolling it away,
//! * auto-hides the bar when stderr is not a TTY (CI-friendly).
//!
//! The `ProgressSink` trait shape (`ohara_core::ProgressSink`) is unchanged.

use std::sync::Mutex;

use indicatif::ProgressStyle;
use ohara_core::ProgressSink;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;

pub struct IndicatifProgress {
    /// The span whose lifetime owns the underlying progress bar.
    ///
    /// Wrapped in a `Mutex<Option<...>>` so `finish()` can drop it through
    /// the `&self` callback signature on `ProgressSink`. Dropping the span
    /// closes the bar; until then the bar stays pinned at the bottom.
    span: Mutex<Option<Span>>,
}

impl IndicatifProgress {
    pub fn new() -> Self {
        // `info_span!` is cheap when no IndicatifLayer is registered (a
        // common case in tests). When the layer *is* registered, the span
        // becomes a progress-bar handle once we call `pb_set_*` on it.
        let span = tracing::info_span!("ohara_index");
        Self {
            span: Mutex::new(Some(span)),
        }
    }

    fn with_span<F: FnOnce(&Span)>(&self, f: F) {
        if let Ok(guard) = self.span.lock() {
            if let Some(s) = guard.as_ref() {
                f(s);
            }
        }
    }
}

impl Default for IndicatifProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressSink for IndicatifProgress {
    fn start(&self, total_commits: usize) {
        let style = ProgressStyle::with_template(
            "{spinner:.green} indexing [{elapsed_precise}] [{bar:40.cyan/blue}] \
             {pos}/{len} commits ({eta}) — {msg}",
        )
        .expect("static progress template is valid")
        .progress_chars("=>-");
        self.with_span(|s| {
            s.pb_set_style(&style);
            s.pb_set_length(total_commits as u64);
            s.pb_set_position(0);
            s.pb_set_message("walking commits");
            // pb_start materializes the bar in the layer's MultiProgress
            // (no-op if already started or layer is absent).
            s.pb_start();
        });
    }

    fn commit_done(&self, commits_done: usize, _total_hunks: usize) {
        self.with_span(|s| s.pb_set_position(commits_done as u64));
    }

    fn phase_symbols(&self) {
        self.with_span(|s| s.pb_set_message("extracting HEAD symbols"));
    }

    fn finish(&self, total_commits: usize, total_hunks: usize, head_symbols: usize) {
        let msg =
            format!("done — {total_commits} commits, {total_hunks} hunks, {head_symbols} symbols");
        self.with_span(|s| s.pb_set_message(&msg));
        // Drop the span so `IndicatifLayer::on_close` finalizes the bar.
        if let Ok(mut guard) = self.span.lock() {
            guard.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::ProgressSink;

    /// Smoke test: the sink must be safe to drive even when no
    /// `IndicatifLayer` is registered (e.g. during unit tests). The
    /// `pb_*` calls become no-ops in that case.
    #[test]
    fn drives_without_layer() {
        let p = IndicatifProgress::new();
        p.start(10);
        p.commit_done(1, 0);
        p.commit_done(5, 0);
        p.phase_symbols();
        p.finish(10, 42, 7);
        // After finish(), additional calls are still no-ops (span is None).
        p.commit_done(11, 0);
    }
}
