//! End-of-run reporting for `ohara index` (issue #90 split): the
//! `--profile` JSON dump and the human-readable summary + per-phase bar
//! chart. Pure formatting — no I/O beyond returning strings.

use ohara_core::PhaseTimings;

/// Render `PhaseTimings` as the JSON object emitted by `--profile`.
/// Pulled out of `run` so the JSON shape is unit-testable without
/// driving a real index pass.
pub fn phase_timings_json(pt: &PhaseTimings) -> String {
    serde_json::to_string(pt).expect("PhaseTimings serializes via derive(Serialize)")
}

/// Plan 31: a one-line warning when the parallel indexer could not
/// persist some commits (e.g. a write that exhausted `busy_timeout`).
/// Returns `None` for a clean run. The caller prints this to stderr and
/// exits non-zero, so the index being incomplete is never silent.
pub fn failed_commits_notice(commits_failed: u64) -> Option<String> {
    if commits_failed == 0 {
        return None;
    }
    let noun = if commits_failed == 1 {
        "commit"
    } else {
        "commits"
    };
    Some(format!(
        "⚠ {commits_failed} {noun} failed to index (see warnings above). \
         The index is incomplete — re-run `ohara index` to fill the gaps."
    ))
}

/// Render a multi-line cosmetic summary printed at the end of
/// `ohara index`. Includes commit/hunk/symbol counts, wall-clock total,
/// and a per-phase bar chart sorted by descending cost so the dominant
/// stage leads. Phases with zero recorded ms are omitted.
///
/// Example output:
///
/// ```text
/// indexed in 47.3s — 1670 commits, 5951 hunks, 36976 HEAD symbols
///
///   embed     38.1s  ████████████████████████████████   80%
///   storage    4.2s  ███                                 9%
///   diff       2.6s  ██                                  5%
///   parse      1.8s  █                                   4%
///   symbols    1.2s  █                                   2%
///   fts       400ms                                     <1%
/// ```
pub fn index_summary_human(
    pt: &PhaseTimings,
    total_ms: u64,
    new_commits: u64,
    new_hunks: u64,
    head_symbols: u64,
) -> String {
    let mut phases: Vec<(&str, u64)> = vec![
        ("walk", pt.commit_walk_ms),
        ("diff", pt.diff_extract_ms),
        ("parse", pt.tree_sitter_parse_ms),
        ("embed", pt.embed_ms),
        ("storage", pt.storage_write_ms),
        ("fts", pt.fts_insert_ms),
        ("symbols", pt.head_symbols_ms),
    ];
    phases.retain(|(_, ms)| *ms > 0);
    phases.sort_by_key(|(_, ms)| std::cmp::Reverse(*ms));

    let mut out = String::new();
    out.push_str(&format!(
        "indexed in {} — {} commit{}, {} hunk{}, {} HEAD symbol{}\n",
        fmt_duration_ms(total_ms),
        new_commits,
        if new_commits == 1 { "" } else { "s" },
        new_hunks,
        if new_hunks == 1 { "" } else { "s" },
        head_symbols,
        if head_symbols == 1 { "" } else { "s" },
    ));
    if phases.is_empty() {
        return out;
    }
    out.push('\n');

    const BAR_WIDTH: usize = 32;
    let max_ms = phases.first().map(|(_, m)| *m).unwrap_or(1).max(1);
    // Anchor the percentage to total_ms so it represents wall-clock
    // share, not "share of the longest phase". A short phase shows
    // its real fraction of the run, not an inflated relative number.
    let pct_denom = total_ms.max(1) as f64;

    for (name, ms) in &phases {
        let ratio = (*ms as f64) / (max_ms as f64);
        let filled = (ratio * BAR_WIDTH as f64).round() as usize;
        let filled = filled.min(BAR_WIDTH);
        let bar: String = "█".repeat(filled);
        let pad: String = " ".repeat(BAR_WIDTH - filled);
        let pct = (*ms as f64) / pct_denom * 100.0;
        let pct_str = if pct < 1.0 {
            "<1%".to_string()
        } else {
            format!("{:.0}%", pct)
        };
        out.push_str(&format!(
            "  {name:<8}{time:>6}  {bar}{pad}  {pct:>3}\n",
            name = name,
            time = fmt_duration_ms(*ms),
            bar = bar,
            pad = pad,
            pct = pct_str,
        ));
    }

    out
}

fn fmt_duration_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod profile_json_tests {
    use super::*;

    #[test]
    fn phase_timings_json_contains_every_field() {
        // Contract: every PhaseTimings field is present in the JSON
        // emitted by --profile. The lead's manual baseline run pastes
        // this output into docs/perf/v0.6-baseline.md, so a missing
        // key here breaks the template downstream.
        let pt = PhaseTimings {
            commit_walk_ms: 1,
            diff_extract_ms: 2,
            tree_sitter_parse_ms: 3,
            embed_ms: 4,
            storage_write_ms: 5,
            fts_insert_ms: 6,
            head_symbols_ms: 7,
            total_diff_bytes: 8,
            total_added_lines: 9,
        };
        let s = phase_timings_json(&pt);
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse JSON");
        for key in [
            "commit_walk_ms",
            "diff_extract_ms",
            "tree_sitter_parse_ms",
            "embed_ms",
            "storage_write_ms",
            "fts_insert_ms",
            "head_symbols_ms",
            "total_diff_bytes",
            "total_added_lines",
        ] {
            assert!(
                v.get(key).is_some(),
                "PhaseTimings JSON must expose `{key}`"
            );
        }
        assert_eq!(v.get("commit_walk_ms").and_then(|x| x.as_u64()), Some(1));
        assert_eq!(v.get("total_added_lines").and_then(|x| x.as_u64()), Some(9));
    }

    fn pt_for_summary() -> PhaseTimings {
        PhaseTimings {
            commit_walk_ms: 0,
            diff_extract_ms: 2_600,
            tree_sitter_parse_ms: 1_800,
            embed_ms: 38_100,
            storage_write_ms: 4_200,
            fts_insert_ms: 400,
            head_symbols_ms: 1_200,
            total_diff_bytes: 0,
            total_added_lines: 0,
        }
    }

    #[test]
    fn index_summary_header_pluralizes_and_renders_total() {
        let s = index_summary_human(&pt_for_summary(), 47_300, 1670, 5951, 36976);
        let header = s.lines().next().expect("header line");
        assert_eq!(
            header,
            "indexed in 47.3s — 1670 commits, 5951 hunks, 36976 HEAD symbols"
        );
    }

    #[test]
    fn index_summary_header_singular_when_count_is_one() {
        let s = index_summary_human(&pt_for_summary(), 1_000, 1, 1, 1);
        let header = s.lines().next().expect("header line");
        assert_eq!(header, "indexed in 1.0s — 1 commit, 1 hunk, 1 HEAD symbol");
    }

    #[test]
    fn index_summary_phases_sorted_descending_by_ms() {
        let s = index_summary_human(&pt_for_summary(), 47_300, 1670, 5951, 36976);
        let phase_lines: Vec<&str> = s
            .lines()
            .filter(|l| l.starts_with("  ") && !l.trim().is_empty())
            .collect();
        let names: Vec<String> = phase_lines
            .iter()
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["embed", "storage", "diff", "parse", "symbols", "fts"]
        );
    }

    #[test]
    fn index_summary_omits_zero_phases() {
        // commit_walk_ms = 0 in the fixture; "walk" must not appear.
        let s = index_summary_human(&pt_for_summary(), 47_300, 1670, 5951, 36976);
        assert!(
            !s.contains("walk "),
            "zero-duration `walk` phase should be omitted; got:\n{s}"
        );
    }

    #[test]
    fn index_summary_pct_uses_lt_one_for_sub_percent_phases() {
        // fts: 400ms / 47_300ms = 0.85% → "<1%"
        let s = index_summary_human(&pt_for_summary(), 47_300, 1670, 5951, 36976);
        let fts_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("fts"))
            .expect("fts line");
        assert!(
            fts_line.ends_with("<1%"),
            "fts (~0.8% of total) should show `<1%`; got: `{fts_line}`"
        );
    }

    #[test]
    fn failed_commits_notice_warns_when_any_failed() {
        let n = failed_commits_notice(3).expect("3 failures must produce a notice");
        assert!(n.contains('3'), "notice must name the count: {n}");
        assert!(
            n.to_lowercase().contains("fail"),
            "notice must say commits failed: {n}"
        );
        assert!(
            n.contains("ohara index"),
            "notice must point at the recovery command: {n}"
        );
    }

    #[test]
    fn failed_commits_notice_singular_for_one() {
        let n = failed_commits_notice(1).expect("1 failure must produce a notice");
        assert!(
            n.contains("1 commit ") && !n.contains("commits"),
            "one failure must read singular: {n}"
        );
    }

    #[test]
    fn failed_commits_notice_silent_when_none() {
        assert!(
            failed_commits_notice(0).is_none(),
            "a clean run must not emit a failure notice"
        );
    }

    #[test]
    fn index_summary_no_phases_just_emits_header() {
        let pt = PhaseTimings {
            commit_walk_ms: 0,
            diff_extract_ms: 0,
            tree_sitter_parse_ms: 0,
            embed_ms: 0,
            storage_write_ms: 0,
            fts_insert_ms: 0,
            head_symbols_ms: 0,
            total_diff_bytes: 0,
            total_added_lines: 0,
        };
        let s = index_summary_human(&pt, 0, 0, 0, 0);
        assert_eq!(s, "indexed in 0ms — 0 commits, 0 hunks, 0 HEAD symbols\n");
    }
}
