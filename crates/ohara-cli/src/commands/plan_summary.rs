//! Cosmetic summary helpers for `ohara plan`: a bar-chart hotmap of the
//! top-level directories by commit share, and a one-line banner that
//! mirrors the closing banner of `ohara index` (`index_summary_human`
//! in `commands::index`). Pure functions over `HotmapAggregator` and
//! primitive inputs so they're unit-testable without a git repo.

use crate::commands::plan::HotmapAggregator;

const BAR_WIDTH: usize = 32;

/// Render the one-line closing banner for `ohara plan` — the analogue
/// of `index_summary_human`'s header, e.g.
/// `surveyed 12345 commits in 8.4s — 3 suggested ignore patterns`.
/// Pluralises commits and patterns independently so the banner stays
/// grammatical at all counts (including zero suggestions).
pub fn render_plan_banner(total_commits: u64, elapsed_ms: u64, suggestion_count: usize) -> String {
    let commits_word = match total_commits {
        1 => "commit",
        _ => "commits",
    };
    let pattern_word = match suggestion_count {
        1 => "suggested ignore pattern",
        _ => "suggested ignore patterns",
    };
    format!(
        "surveyed {total_commits} {commits_word} in {elapsed} — {suggestion_count} {pattern_word}\n",
        elapsed = fmt_duration_ms(elapsed_ms),
    )
}

fn fmt_duration_ms(ms: u64) -> String {
    match ms >= 1000 {
        true => format!("{:.1}s", ms as f64 / 1000.0),
        false => format!("{ms}ms"),
    }
}

/// Render the section header plus a bar-chart of the `top_n` top-level
/// directories by commit share, sorted descending. Format mirrors
/// `index_summary_human`: `█`-filled bars of width [`BAR_WIDTH`], with
/// `<1%` used for sub-percent shares so a small directory still has a
/// recognisable row instead of `0%`.
///
/// Always emits the header line. With an empty aggregator (or no
/// top-level rows) only the header is produced — never panics on
/// division by zero.
pub fn render_hotmap_bars(agg: &HotmapAggregator, top_n: usize) -> String {
    let mut out = String::from("top-level directories by commit share:\n");

    let mut rows: Vec<(&String, &u64)> = agg
        .counts()
        .iter()
        .filter(|(k, _)| k.matches('/').count() == 1 && k.ends_with('/'))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    rows.truncate(top_n);

    if rows.is_empty() {
        return out;
    }

    let name_pad = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let max_count = rows.first().map(|(_, c)| **c).unwrap_or(1).max(1);
    let total = agg.total_commits().max(1) as f64;

    for (name, count) in &rows {
        let ratio = (**count as f64) / (max_count as f64);
        let filled = ((ratio * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
        let bar: String = "█".repeat(filled);
        let bar_pad: String = " ".repeat(BAR_WIDTH - filled);
        let pct = (**count as f64) / total * 100.0;
        let pct_str = match pct < 1.0 {
            true => "<1%".to_string(),
            false => format!("{:.0}%", pct),
        };
        out.push_str(&format!(
            "  {name:<name_pad$}  {count:>7}  {bar}{bar_pad}  {pct:>3}\n",
            name = name,
            name_pad = name_pad,
            count = count,
            bar = bar,
            bar_pad = bar_pad,
            pct = pct_str,
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_dir_aggregator() -> HotmapAggregator {
        // drivers/ 60%, src/ 30%, vendor/ 10%
        let mut agg = HotmapAggregator::default();
        for _ in 0..60 {
            agg.record(&["drivers/foo.c".into()]);
        }
        for _ in 0..30 {
            agg.record(&["src/main.rs".into()]);
        }
        for _ in 0..10 {
            agg.record(&["vendor/bar.rs".into()]);
        }
        agg
    }

    #[test]
    fn render_hotmap_bars_emits_section_header_and_top_dirs_descending() {
        let agg = three_dir_aggregator();
        let out = render_hotmap_bars(&agg, 20);
        assert!(
            out.starts_with("top-level directories by commit share:"),
            "expected section header, got: {out}"
        );
        let drivers_pos = out.find("drivers/").expect("drivers/ row present");
        let src_pos = out.find("src/").expect("src/ row present");
        let vendor_pos = out.find("vendor/").expect("vendor/ row present");
        assert!(
            drivers_pos < src_pos && src_pos < vendor_pos,
            "rows must be sorted descending by commit share: {out}"
        );
    }

    #[test]
    fn render_hotmap_bars_uses_lt_one_for_sub_percent_dir() {
        // 1 commit in niche/, 999 in src/ — niche is 0.1% share.
        let mut agg = HotmapAggregator::default();
        agg.record(&["niche/x.rs".into()]);
        for _ in 0..999 {
            agg.record(&["src/main.rs".into()]);
        }
        let out = render_hotmap_bars(&agg, 20);
        let niche_line = out
            .lines()
            .find(|l| l.contains("niche/"))
            .unwrap_or_else(|| panic!("niche/ row missing in: {out}"));
        assert!(
            niche_line.contains("<1%"),
            "sub-percent row must render <1%, got: {niche_line}"
        );
    }

    #[test]
    fn render_hotmap_bars_excludes_nested_paths() {
        // Recording `drivers/staging/foo.c` populates counts() with
        // `drivers/`, `drivers/staging/`, and `drivers/staging/foo.c` —
        // only the top-level `drivers/` belongs in the hotmap.
        let mut agg = HotmapAggregator::default();
        agg.record(&["drivers/staging/foo.c".into()]);
        let out = render_hotmap_bars(&agg, 20);
        assert!(out.contains("drivers/"), "drivers/ row expected: {out}");
        assert!(
            !out.contains("drivers/staging/"),
            "nested drivers/staging/ row must not be rendered: {out}"
        );
    }

    #[test]
    fn render_hotmap_bars_respects_top_n() {
        let agg = three_dir_aggregator();
        let out = render_hotmap_bars(&agg, 2);
        // Header + 2 rows; vendor/ (the smallest share) drops out.
        assert!(out.contains("drivers/") && out.contains("src/"));
        assert!(
            !out.contains("vendor/"),
            "top_n=2 must drop the third-place row: {out}"
        );
    }

    #[test]
    fn render_plan_banner_pluralizes_commits_and_patterns() {
        let singular = render_plan_banner(1, 1_000, 1);
        assert!(
            singular.contains("1 commit ")
                || singular.contains("1 commit\n")
                || singular.contains("1 commit "),
            "singular commit form expected: {singular}"
        );
        assert!(
            !singular.contains("1 commits"),
            "singular must not pluralise 'commit': {singular}"
        );
        assert!(
            singular.contains("1 suggested ignore pattern")
                && !singular.contains("1 suggested ignore patterns"),
            "singular pattern form expected: {singular}"
        );

        let plural = render_plan_banner(5, 1_000, 3);
        assert!(
            plural.contains("5 commits"),
            "plural commits form expected: {plural}"
        );
        assert!(
            plural.contains("3 suggested ignore patterns"),
            "plural patterns form expected: {plural}"
        );
    }

    #[test]
    fn render_plan_banner_zero_suggestions_says_zero() {
        let out = render_plan_banner(100, 1_000, 0);
        assert!(
            out.contains("0 suggested ignore patterns"),
            "zero-suggestion banner expected: {out}"
        );
        assert!(
            out.contains("100 commits"),
            "commits count must still appear: {out}"
        );
    }

    #[test]
    fn render_plan_banner_uses_seconds_format_above_one_second() {
        let above = render_plan_banner(10, 8_400, 2);
        assert!(
            above.contains("8.4s"),
            "ms >= 1000 should render as seconds: {above}"
        );
        let below = render_plan_banner(10, 420, 2);
        assert!(
            below.contains("420ms"),
            "ms < 1000 should render as ms: {below}"
        );
    }

    #[test]
    fn render_hotmap_bars_empty_aggregator_emits_only_header() {
        let agg = HotmapAggregator::default();
        let out = render_hotmap_bars(&agg, 20);
        assert!(out.starts_with("top-level directories by commit share:"));
        // No row content beyond the header line + trailing newline.
        let extra: Vec<&str> = out
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert!(
            extra.is_empty(),
            "empty aggregator must produce no rows, got extras: {extra:?}"
        );
    }
}
