//! Cosmetic summary helpers for `ohara plan`: a bar-chart hotmap of the
//! top-level directories by commit share, and a one-line banner that
//! mirrors the closing banner of `ohara index` (`index_summary_human`
//! in `commands::index`). Pure functions over `HotmapAggregator` and
//! primitive inputs so they're unit-testable without a git repo.

use crate::commands::plan::HotmapAggregator;

const BAR_WIDTH: usize = 32;

/// Render the section header plus a bar-chart of the `top_n` top-level
/// directories by commit share, sorted descending. Format mirrors
/// `index_summary_human`: `█`-filled bars of width [`BAR_WIDTH`], with
/// `<1%` used for sub-percent shares so a small directory still has a
/// recognisable row instead of `0%`.
///
/// Always emits the header line. With an empty aggregator (or no
/// top-level rows) only the header is produced — never panics on
/// division by zero.
pub fn render_hotmap_bars(_agg: &HotmapAggregator, _top_n: usize) -> String {
    String::new()
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
