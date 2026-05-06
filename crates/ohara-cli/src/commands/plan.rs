//! `ohara plan` — pre-flight planner that surveys the repo and prints a
//! directory commit-share hotmap plus suggested `.oharaignore` patterns.
//!
//! Plan-26 / Spec A introduced this as an auto-writer; issue #37
//! demoted it to **print-only by default**. The `--write` flag opts
//! back into writing `.oharaignore` at the repo root (marker-fenced,
//! merged with user-added lines below the markers).
//!
//! The file lives at the repo root (not `.ohara/`) so it's checked
//! into the repo and shared across the team like `.gitignore`.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use ohara_git::GitWalker;
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Apply the suggestions to `.oharaignore` (default: print only).
    /// Merges into any existing marker block; preserves user-added
    /// lines below the closing marker.
    #[arg(long)]
    pub write: bool,
    /// Replace the entire `.oharaignore` (default merge replaces only
    /// the auto-generated section between markers, preserving user
    /// lines). Only meaningful with `--write`.
    #[arg(long, requires = "write")]
    pub replace: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let canonical = std::fs::canonicalize(&args.path)
        .with_context(|| format!("canonicalize {}", args.path.display()))?;

    println!("walking commit history (paths only)…");
    let walker = GitWalker::open(&canonical).context("open git repo")?;

    let start = std::time::Instant::now();
    let mut agg = HotmapAggregator::default();
    walker.for_each_commit_paths(|_meta, paths| {
        agg.record(paths);
        Ok(())
    })?;
    let elapsed = start.elapsed();
    println!(
        "walked {} commits in {:.1}s",
        agg.total_commits(),
        elapsed.as_secs_f64()
    );

    print_hotmap(&agg);
    let suggestions = suggest_patterns(&agg);
    print_suggestions(&suggestions, agg.total_commits());
    print_gpu_hint();

    if !args.write {
        // Issue #37: print-only by default. The previous auto-write
        // silently excluded engine directories on repos where the
        // most-touched top-level dir was the engine itself.
        println!("\nre-run with --write to apply these suggestions to .oharaignore");
        return Ok(());
    }

    let target = canonical.join(".oharaignore");
    let new_section = render_oharaignore_body(&suggestions, VERSION);

    let final_text = if args.replace || !target.exists() {
        new_section
    } else {
        let existing = std::fs::read_to_string(&target)
            .with_context(|| format!("read {}", target.display()))?;
        merge_oharaignore(&existing, &new_section)?
    };

    std::fs::write(&target, final_text).with_context(|| format!("write {}", target.display()))?;
    println!("wrote {}", target.display());
    Ok(())
}

/// Print the top-N directories by commit share.
fn print_hotmap(agg: &HotmapAggregator) {
    let total = agg.total_commits().max(1);
    let mut top: Vec<(&String, &u64)> = agg
        .counts()
        .iter()
        .filter(|(k, _)| {
            let slash_count = k.matches('/').count();
            slash_count == 1 && k.ends_with('/')
        })
        .collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!("\ntop-level directories by commit share:");
    for (k, count) in top.iter().take(20) {
        let share = (**count as f64 / total as f64) * 100.0;
        println!("  {:<40} {:>7} ({:>4.1}%)", k, count, share);
    }
}

fn print_suggestions(suggestions: &[String], total: u64) {
    println!("\nproposed auto-generated section:");
    if suggestions.is_empty() {
        println!("  (no high-share top-level directories — nothing suggested)");
    }
    for s in suggestions {
        println!("  {s}");
    }
    println!("\ntotal commits surveyed: {total}");
}

fn print_gpu_hint() {
    let coreml = cfg!(feature = "coreml");
    let cuda = cfg!(feature = "cuda");
    if coreml || cuda {
        println!(
            "\nnote: ohara is built with --features {} ; embedding will use the accelerator.",
            if coreml { "coreml" } else { "cuda" }
        );
    } else {
        println!(
            "\nnote: rebuild with --features coreml (Apple) or --features cuda (NVIDIA) for ~3-5x embed speedup."
        );
    }
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
    let total = agg.total_commits() as f64;
    if total <= 0.0 {
        return Vec::new();
    }
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
        // Compare share in f64 — the previous integer cast floored
        // `total * 0.05` to 0 on repos with fewer than 20 commits,
        // which made every directory match `count >= 0`. See #41.
        if (*count as f64) / total >= HIGH_SHARE_THRESHOLD {
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

    #[test]
    fn sub_threshold_share_with_small_total_is_not_suggested() {
        // Issue #41 (bug 1): the integer-floored threshold
        // `(total * 0.05) as u64` rounded down on small repos.
        // 30 commits / 1 niche/ commit = 3.3% share, below 5%, but
        // the buggy `(30.0 * 0.05) as u64 = 1` made `1 >= 1` match.
        let mut agg = HotmapAggregator::default();
        agg.record(&["niche/foo.rs".into()]);
        for _ in 0..29 {
            agg.record(&["src/main.rs".into()]);
        }
        let suggestions = suggest_patterns(&agg);
        assert!(
            !suggestions.iter().any(|p| p == "niche/"),
            "niche/ at 3.3% share must not be suggested; got {suggestions:?}"
        );
    }
}

const MARKER_BEGIN_PREFIX: &str = "# === ohara plan v";
const MARKER_END: &str = "# === end auto-generated ===";

/// Public for tests; the live opener prepended in `render_oharaignore_body`.
pub const MARKER_BEGIN: &str = "# === ohara plan v";

/// Render the body of a fresh `.oharaignore`: marker-fenced patterns
/// followed by a hint for user-added lines below the closing marker.
pub fn render_oharaignore_body(patterns: &[String], version: &str) -> String {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let mut out = String::new();
    out.push_str(&format!(
        "{MARKER_BEGIN_PREFIX}{version} — auto-generated {timestamp} ===\n"
    ));
    for p in patterns {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str(MARKER_END);
    out.push('\n');
    out.push('\n');
    out.push_str(
        "# user-added lines below this marker are preserved across `ohara plan` re-runs (use --replace to overwrite the entire file)\n",
    );
    out
}

/// Merge a freshly-rendered auto-section with an existing
/// `.oharaignore`. Replaces only the block between the markers; lines
/// outside are kept verbatim. Errors if the existing file is non-empty
/// and lacks markers (fail-open: refuse to silently overwrite user
/// content).
pub fn merge_oharaignore(existing: &str, new_section: &str) -> Result<String> {
    let trimmed = existing.trim();
    if trimmed.is_empty() {
        return Ok(new_section.to_string());
    }
    let begin = existing.find(MARKER_BEGIN_PREFIX).ok_or_else(|| {
        anyhow::anyhow!(
            "existing .oharaignore has content but no auto-generated markers; \
             pass --replace to overwrite or delete the file and re-run"
        )
    })?;
    // Scope the end-marker search to the region after the begin marker
    // so user prose above the markers can quote the end-marker text
    // without corrupting the splice. See #41.
    let end_relative = existing[begin..].find(MARKER_END).ok_or_else(|| {
        anyhow::anyhow!(
            "existing .oharaignore has begin marker but no end marker; refusing to merge"
        )
    })?;
    let end = begin + end_relative + MARKER_END.len();

    // Walk past trailing whitespace/newline of the end marker line.
    let after_end = existing[end..]
        .find('\n')
        .map(|i| end + i + 1)
        .unwrap_or(end);

    let prefix = &existing[..begin];
    let suffix = &existing[after_end..];

    let mut out = String::new();
    out.push_str(prefix);
    out.push_str(new_section);
    out.push_str(suffix);
    Ok(out)
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn render_oharaignore_wraps_patterns_in_markers() {
        // Plan 26 Task D.4: the auto-generated section is fenced by
        // begin/end markers so re-runs replace only that block.
        let body = render_oharaignore_body(&["drivers/".into(), "vendor/".into()], "0.7.7");
        assert!(body.contains(MARKER_BEGIN));
        assert!(body.contains(MARKER_END));
        assert!(body.contains("drivers/"));
        assert!(body.contains("vendor/"));
        // The opening marker must include the version so a future ohara
        // can detect schema drift.
        assert!(body.contains("ohara plan v0.7.7"));
    }

    #[test]
    fn merge_replaces_only_auto_section_in_existing_file() {
        // Plan 26 Task D.4: default merge preserves user lines outside
        // the markers across re-runs (no flag needed; --replace opts out).
        let existing = "\
# === ohara plan v0.7.6 — auto-generated 2026-05-04T12:00:00 ===
old_pattern/
# === end auto-generated ===

# user added below
my_team/
!Cargo.lock
";
        let new_section = render_oharaignore_body(&["drivers/".into()], "0.7.7");
        let merged = merge_oharaignore(existing, &new_section).expect("merge");

        assert!(merged.contains("drivers/"), "new pattern present");
        assert!(!merged.contains("old_pattern/"), "old auto pattern dropped");
        assert!(merged.contains("my_team/"), "user line preserved");
        assert!(merged.contains("!Cargo.lock"), "user negation preserved");
    }

    #[test]
    fn merge_fails_open_when_markers_missing() {
        // Plan 26 Task D.4: refusing to overwrite an existing file
        // without markers protects user lines from silent loss.
        let existing = "user_only_pattern/\n";
        let new_section = render_oharaignore_body(&["drivers/".into()], "0.7.7");
        let res = merge_oharaignore(existing, &new_section);
        assert!(res.is_err(), "merge must refuse when markers absent");
    }

    #[test]
    fn merge_ignores_stray_end_marker_above_begin() {
        // Issue #41 (bug 2): the unscoped `find(MARKER_END)` returned
        // the first occurrence even when it sat in user prose ABOVE
        // the real begin marker, scrambling the splice.
        let existing = "\
# notes for teammates:
#   the auto-generated block ends with a line that looks like
#   `# === end auto-generated ===`
# do not edit that block by hand
# === ohara plan v0.7.6 — auto-generated 2026-05-04T12:00:00 ===
old_pattern/
# === end auto-generated ===

# user added below
my_team/
!Cargo.lock
";
        let new_section = render_oharaignore_body(&["drivers/".into()], "0.7.7");
        let merged = merge_oharaignore(existing, &new_section).expect("merge");

        assert!(merged.contains("drivers/"), "new pattern present");
        assert!(!merged.contains("old_pattern/"), "old auto pattern dropped");
        assert!(
            merged.contains("notes for teammates:"),
            "user prose above markers preserved: {merged}"
        );
        assert!(
            merged.contains("do not edit that block by hand"),
            "user prose above markers preserved verbatim: {merged}"
        );
        assert!(merged.contains("my_team/"), "user lines below preserved");
        assert!(
            merged.contains("!Cargo.lock"),
            "user negation below preserved"
        );
        // Exactly one begin marker after merge — the buggy splice can
        // leave the original begin marker AND prepend a fresh one.
        assert_eq!(
            merged.matches(MARKER_BEGIN).count(),
            1,
            "exactly one begin marker after merge, got: {merged}"
        );
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
