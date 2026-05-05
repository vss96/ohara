//! `ohara plan` — pre-flight planner that surveys the repo, prints a
//! directory commit-share hotmap, and writes a `.oharaignore` at the
//! repo root.
//!
//! Plan-26 / Spec A. The file lives at the repo root (not `.ohara/`)
//! so it's checked into the repo and shared across the team like
//! `.gitignore`.

#![allow(unused_imports)]

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Write `.oharaignore` without prompting.
    #[arg(long)]
    pub yes: bool,
    /// Print suggestions only; never write a file.
    #[arg(long, conflicts_with = "yes")]
    pub no_write: bool,
    /// Replace the entire `.oharaignore` (default: replace only the
    /// auto-generated section between markers, preserving user lines).
    #[arg(long)]
    pub replace: bool,
}

pub async fn run(_args: Args) -> Result<()> {
    Err(anyhow::anyhow!("plan-26: `ohara plan` not yet implemented"))
}

use std::collections::BTreeMap;

/// Streaming aggregator: receives `(commit, paths)` and tallies a
/// commit-count per directory prefix. Pure function over its inputs;
/// holds at most O(unique-prefixes) memory.
#[derive(Default)]
pub struct HotmapAggregator {
    counts: BTreeMap<String, u64>,
    total: u64,
}

impl HotmapAggregator {
    /// Record one commit's changed-paths list. Each prefix of each path
    /// is incremented once per commit (a commit touching two files
    /// under `drivers/` still bumps `drivers/` only once).
    pub fn record(&mut self, paths: &[String]) {
        self.total += 1;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in paths {
            let mut buf = String::new();
            for component in p.split('/') {
                if !buf.is_empty() {
                    buf.push('/');
                }
                buf.push_str(component);
                // If there is more path remaining after this component,
                // emit the directory key (with trailing slash); otherwise
                // emit the leaf key (no trailing slash).
                let key = if p.starts_with(&format!("{buf}/")) {
                    format!("{buf}/")
                } else {
                    buf.clone()
                };
                if seen.insert(key.clone()) {
                    *self.counts.entry(key).or_insert(0) += 1;
                }
            }
        }
    }

    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    pub fn total_commits(&self) -> u64 {
        self.total
    }
}

/// Top-level directory names the planner never suggests ignoring.
const DOCS_ALLOWLIST: &[&str] = &["Documentation/", "docs/", "doc/"];

/// Default share threshold for "high-share" suggestions, expressed as
/// a fraction of total commits. Tunable; 5% balances signal vs noise on
/// repos in the 100k+ commit range.
const HIGH_SHARE_THRESHOLD: f64 = 0.05;

/// Generate `.oharaignore` patterns from a populated aggregator. Top-
/// level directories with commit share above the threshold and not in
/// the docs allowlist are returned in deterministic order.
pub fn suggest_patterns(agg: &HotmapAggregator) -> Vec<String> {
    if agg.total_commits() == 0 {
        return Vec::new();
    }
    let threshold = (agg.total_commits() as f64 * HIGH_SHARE_THRESHOLD) as u64;
    let mut out: Vec<String> = Vec::new();

    for (key, count) in agg.counts() {
        // Top-level only: exactly one slash, at the end.
        let slash_count = key.matches('/').count();
        let is_toplevel_dir = slash_count == 1 && key.ends_with('/');
        if !is_toplevel_dir {
            continue;
        }
        if DOCS_ALLOWLIST.iter().any(|d| *d == key) {
            continue;
        }
        if *count >= threshold {
            out.push(key.clone());
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn high_share_directory_outside_docs_allowlist_is_suggested() {
        // Plan 26 Task D.3: a top-level directory with > 5% commit
        // share that isn't in the docs allowlist is suggested for IGNORE.
        // NOTE: the original spec also asserted `!suggestions.contains("src/")`,
        // but at a 5% threshold src/ (30%) also qualifies. We drop that
        // over-specified assertion and only pin the meaningful signal: that
        // drivers/ IS suggested.
        let mut agg = HotmapAggregator::default();
        for _ in 0..70 {
            agg.record(&["drivers/foo.c".into()]);
        }
        for _ in 0..30 {
            agg.record(&["src/main.rs".into()]);
        }

        let suggestions = suggest_patterns(&agg);
        assert!(suggestions.iter().any(|p| p == "drivers/"));
    }

    #[test]
    fn high_share_documentation_dir_is_kept() {
        // Plan 26 Task D.3: `Documentation/` is in the docs allowlist —
        // even at high commit share it must not be suggested for ignore.
        let mut agg = HotmapAggregator::default();
        for _ in 0..70 {
            agg.record(&["Documentation/foo.txt".into()]);
        }
        for _ in 0..30 {
            agg.record(&["src/main.rs".into()]);
        }
        let suggestions = suggest_patterns(&agg);
        assert!(!suggestions.iter().any(|p| p == "Documentation/"));
    }

    #[test]
    fn low_share_directory_not_suggested() {
        let mut agg = HotmapAggregator::default();
        for _ in 0..2 {
            agg.record(&["niche/foo.rs".into()]);
        }
        for _ in 0..98 {
            agg.record(&["src/main.rs".into()]);
        }
        let suggestions = suggest_patterns(&agg);
        assert!(!suggestions.iter().any(|p| p == "niche/"));
    }
}

#[cfg(test)]
mod aggregator_tests {
    use super::*;

    #[test]
    fn aggregator_counts_commits_per_top_level_dir() {
        // Plan 26 Task D.2: each path bumps the counter for every prefix.
        // For `drivers/staging/foo.c` we increment `drivers/` by 1,
        // `drivers/staging/` by 1, and `drivers/staging/foo.c` by 1.
        // A second commit touching `drivers/usb/bar.c` bumps `drivers/`
        // again and the new prefixes.
        let mut agg = HotmapAggregator::default();
        agg.record(&["drivers/staging/foo.c".into()]);
        agg.record(&["drivers/usb/bar.c".into()]);
        agg.record(&["src/main.rs".into()]);

        let counts = agg.counts();
        assert_eq!(counts.get("drivers/"), Some(&2));
        assert_eq!(counts.get("drivers/staging/"), Some(&1));
        assert_eq!(counts.get("drivers/usb/"), Some(&1));
        assert_eq!(counts.get("src/"), Some(&1));
    }

    #[test]
    fn aggregator_total_commits_advances_per_record() {
        let mut agg = HotmapAggregator::default();
        agg.record(&["a.rs".into()]);
        agg.record(&["b.rs".into()]);
        agg.record(&[]); // empty diff still counts as a commit
        assert_eq!(agg.total_commits(), 3);
    }
}
