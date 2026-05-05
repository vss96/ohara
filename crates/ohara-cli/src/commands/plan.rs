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
