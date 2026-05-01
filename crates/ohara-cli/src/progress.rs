//! `indicatif`-backed `ProgressSink` for the `ohara index` CLI command.
//!
//! Renders a progress bar on stderr only when stderr is a TTY (so CI
//! logs don't get a stream of `\r`-rewritten lines). When stderr is not
//! a TTY, the bar stays hidden — the existing `tracing::info!` events
//! the indexer already emits carry liveness through to log aggregators.

use indicatif::{ProgressBar, ProgressStyle};
use ohara_core::ProgressSink;

pub struct IndicatifProgress {
    /// indicatif's `ProgressBar` is internally `Arc<...>`, so all
    /// `set_*` methods take `&self` — we can hold one bar by value
    /// and mutate it from the `ProgressSink` callbacks.
    bar: ProgressBar,
}

impl IndicatifProgress {
    pub fn new() -> Self {
        let bar = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            ProgressBar::new(0)
        } else {
            ProgressBar::hidden()
        };
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} indexing [{elapsed_precise}] [{bar:40.cyan/blue}] \
                 {pos}/{len} commits ({eta}) — {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        Self { bar }
    }
}

impl Default for IndicatifProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressSink for IndicatifProgress {
    fn start(&self, total_commits: usize) {
        self.bar.set_length(total_commits as u64);
        self.bar.set_position(0);
        self.bar.set_message("walking commits");
        self.bar
            .enable_steady_tick(std::time::Duration::from_millis(120));
    }

    fn commit_done(&self, commits_done: usize, _total_hunks: usize) {
        self.bar.set_position(commits_done as u64);
    }

    fn phase_symbols(&self) {
        self.bar.set_message("extracting HEAD symbols");
    }

    fn finish(&self, total_commits: usize, total_hunks: usize, head_symbols: usize) {
        self.bar.finish_with_message(format!(
            "done — {total_commits} commits, {total_hunks} hunks, {head_symbols} symbols"
        ));
    }
}
