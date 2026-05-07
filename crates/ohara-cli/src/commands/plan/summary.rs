//! Cosmetic end-of-run summary for `ohara plan` — issue #67.
//!
//! Mirrors the visual language of [`crate::commands::index::index_summary_human`]
//! so the two pre-flight commands feel like the same family: a header
//! banner, a fixed-width bar chart of the dominant signal sorted
//! descending, and the same `<1%` rule for sub-percent rows.
//!
//! For `ohara plan` the bar chart is the per-top-level-directory commit
//! share (the "hotmap"), and the banner counts `--write`-eligible
//! suggestions instead of phase totals.
//!
//! The renderer is pure: input is `&HotmapAggregator` + elapsed
//! wall-time + suggestions list, output is a `String`. That keeps it
//! unit-testable without a git repo.

use super::HotmapAggregator;

/// Render the multi-line cosmetic summary printed at the end of
/// `ohara plan`. Pure function over its inputs.
///
/// Example output:
///
/// ```text
/// surveyed 1670 commits across 12 paths in 8.4s — 2 suggested ignore patterns
///
///   noise/      ████████████████████████████████   91%
///   src/        ███                                 9%
///
/// suggested (.oharaignore patterns):
///   noise/
///   vendor/
/// ```
#[allow(dead_code)] // stub: wired into `run` in the matching green commit.
pub fn plan_summary_human(
    _agg: &HotmapAggregator,
    _elapsed_ms: u64,
    _suggestions: &[String],
) -> String {
    // Stub: real implementation lands in the matching green commit.
    // Tests in this module are the format contract; they MUST fail
    // against the stub and pass against the impl.
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg_with(entries: &[(&str, u64)], total: u64) -> HotmapAggregator {
        // Build an aggregator with exact `(key, count)` entries by
        // recording synthetic commits. We can't poke private fields
        // from this module (they're private to the parent), so we
        // drive `record` instead. Each entry's `count` recorded
        // commits all touch a unique file under that prefix; the
        // remaining commits go to a sentinel file under `__pad/`.
        let mut agg = HotmapAggregator::default();
        let mut recorded: u64 = 0;
        for (key, count) in entries {
            // `key` has the trailing slash because it represents a
            // directory; `record` wants a real file path.
            for i in 0..*count {
                agg.record(&[format!("{key}f{i}.txt")]);
                recorded += 1;
            }
        }
        // Pad up to `total` with commits that don't bump any of the
        // entries' top-level dirs.
        for i in recorded..total {
            agg.record(&[format!("__pad/{i}.txt")]);
        }
        agg
    }

    #[test]
    fn summary_header_pluralizes_and_renders_total() {
        // Pin: header sentence shape so future tweaks don't drift.
        let agg = agg_with(&[("noise/", 91), ("src/", 9)], 100);
        let suggestions = vec!["noise/".to_string()];
        let s = plan_summary_human(&agg, 8_400, &suggestions);
        let header = s.lines().next().expect("header line");
        assert_eq!(
            header,
            "surveyed 100 commits across 2 paths in 8.4s — 1 suggested ignore pattern"
        );
    }

    #[test]
    fn summary_header_singular_when_count_is_one() {
        // A 1-commit repo with one path and zero suggestions — every
        // count must singularise.
        let agg = agg_with(&[("solo/", 1)], 1);
        let s = plan_summary_human(&agg, 50, &[]);
        let header = s.lines().next().expect("header line");
        assert_eq!(
            header,
            "surveyed 1 commit across 1 path in 50ms — 0 suggested ignore patterns"
        );
    }

    #[test]
    fn summary_paths_sorted_descending_by_share() {
        // The bar chart MUST lead with the dominant directory, even if
        // the `BTreeMap`-backed counts iterate alphabetically.
        let agg = agg_with(&[("zoo/", 80), ("alpha/", 20)], 100);
        let s = plan_summary_human(&agg, 1_000, &[]);
        let chart_lines: Vec<String> = s
            .lines()
            .filter(|l| {
                l.starts_with("  ") && (l.contains('\u{2588}') || l.trim_end().ends_with('%'))
            })
            .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
            .collect();
        assert_eq!(chart_lines, vec!["zoo/", "alpha/"]);
    }

    #[test]
    fn summary_pct_uses_lt_one_for_sub_percent_rows() {
        // 1 commit out of 200 = 0.5% — must render as `<1%`, mirroring
        // `index_summary_human`'s rule.
        let agg = agg_with(&[("rare/", 1), ("bulk/", 199)], 200);
        let s = plan_summary_human(&agg, 1_000, &[]);
        let rare_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("rare/"))
            .expect("rare/ line");
        assert!(
            rare_line.trim_end().ends_with("<1%"),
            "rare/ at 0.5% must show `<1%`, got: `{rare_line}`"
        );
    }

    #[test]
    fn summary_includes_suggestions_block() {
        // The suggestions list (the actual `--write`-eligible patterns)
        // appears below the chart so it's the last thing on screen and
        // the user's eye lands on it.
        let agg = agg_with(&[("noise/", 90), ("src/", 10)], 100);
        let suggestions = vec!["noise/".to_string()];
        let s = plan_summary_human(&agg, 1_000, &suggestions);
        assert!(
            s.contains("suggested (.oharaignore patterns):"),
            "suggestions header missing; got:\n{s}"
        );
        // `noise/` appears twice: once in the bar chart, once in the
        // suggestions block. We only assert the latter shape.
        assert!(
            s.lines().any(|l| l == "  noise/"),
            "indented suggestion line missing; got:\n{s}"
        );
    }

    #[test]
    fn summary_empty_suggestions_emits_nothing_suggested_note() {
        // When no directory crosses the 5% threshold, the suggestions
        // block becomes a single explanatory line — same data shape
        // as the legacy `print_suggestions` empty branch, just less
        // shouty.
        let agg = agg_with(&[("src/", 100)], 100);
        let s = plan_summary_human(&agg, 1_000, &[]);
        assert!(
            s.contains("nothing suggested"),
            "empty-suggestions hint missing; got:\n{s}"
        );
        assert!(
            !s.contains("suggested (.oharaignore patterns):"),
            "must not emit suggestions header when list is empty; got:\n{s}"
        );
    }

    #[test]
    fn summary_no_directories_just_emits_header() {
        // Edge case: an empty repo (no commits) renders only the header
        // plus the empty-suggestions hint — never an empty chart block
        // with stray blank lines.
        let agg = HotmapAggregator::default();
        let s = plan_summary_human(&agg, 0, &[]);
        // No bar character anywhere.
        assert!(
            !s.contains('\u{2588}'),
            "empty aggregator must not render any bar; got:\n{s}"
        );
        assert!(s.starts_with("surveyed 0 commits"));
    }
}
