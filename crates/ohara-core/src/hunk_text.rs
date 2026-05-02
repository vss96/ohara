//! Plan 11 — semantic-text builder for hunks.
//!
//! The embedder + the new `fts_hunk_semantic` BM25 lane see this
//! representation in place of raw `diff_text`. The shape is a
//! deliberately ordered set of sections so the embedder + BM25 both
//! get a high-signal-density view of the change:
//!
//! ```text
//! commit: <message first line>
//! file: <file path>
//! language: <language tag, when known>
//! symbols: <comma-separated symbol names that this hunk touched>
//! change: <added | modified | deleted>
//! added_lines:
//! <every '+'-prefixed line, with the leading '+' stripped>
//! ```
//!
//! Sections that don't apply (no symbols, no language) are skipped
//! rather than emitted blank — keeps the BM25 vocabulary small.
//!
//! Raw `diff_text` is preserved on the parent `Hunk` for display /
//! provenance; this representation is search-time only.

use crate::types::{ChangeKind, Hunk, HunkSymbol};

/// Produce the semantic-text representation for `hunk` against the
/// caller-supplied commit message + per-hunk symbol attribution.
///
/// Empty result means "fall back to raw diff text" — caller decides
/// whether to use this or `hunk.diff_text` based on emptiness.
pub fn build(hunk: &Hunk, commit_message: &str, symbols: &[HunkSymbol]) -> String {
    let mut sections: Vec<String> = Vec::with_capacity(6);

    let first_line = commit_message.lines().next().unwrap_or("");
    if !first_line.is_empty() {
        sections.push(format!("commit: {first_line}"));
    }

    sections.push(format!("file: {}", hunk.file_path));

    if let Some(lang) = &hunk.language {
        sections.push(format!("language: {lang}"));
    }

    if !symbols.is_empty() {
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        sections.push(format!("symbols: {}", names.join(", ")));
    }

    sections.push(format!(
        "change: {}",
        match hunk.change_kind {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
        }
    ));

    let added = extract_added_lines(&hunk.diff_text);
    if !added.is_empty() {
        sections.push(format!("added_lines:\n{added}"));
    }

    sections.join("\n")
}

/// Extract the body of `+`-prefixed content lines, stripping the leading
/// `+`. Drops the `+++ b/path` file header (still `+`-prefixed but it's
/// metadata, not content) and the `@@` hunk-header lines. Removes
/// deletions and unchanged context — the embedder only sees what was
/// genuinely added.
fn extract_added_lines(diff_text: &str) -> String {
    let mut out = String::new();
    for line in diff_text.lines() {
        if line.starts_with("+++") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(rest);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AttributionKind, ChangeKind, SymbolKind};

    fn hunk(diff_text: &str, lang: Option<&str>, change: ChangeKind) -> Hunk {
        Hunk {
            commit_sha: "c1".into(),
            file_path: "src/fetch.rs".into(),
            language: lang.map(str::to_string),
            change_kind: change,
            diff_text: diff_text.into(),
        }
    }

    fn sym(name: &str, attribution: AttributionKind) -> HunkSymbol {
        HunkSymbol {
            kind: SymbolKind::Function,
            name: name.into(),
            qualified_name: None,
            attribution,
        }
    }

    #[test]
    fn build_emits_sections_in_order() {
        // Plan 11 Task 2.1 Step 1: section order is part of the
        // contract — callers comparing across two index passes can
        // diff the strings byte-for-byte.
        let h = hunk(
            "--- a/x.rs\n+++ b/x.rs\n@@\n+fn fetch() {}\n+    retry();\n context\n-deleted\n",
            Some("rust"),
            ChangeKind::Added,
        );
        let body = build(
            &h,
            "fetch: add retry with exponential backoff",
            &[sym("fetch", AttributionKind::ExactSpan)],
        );
        // Find each section's index in the output; assert ascending.
        let positions: Vec<_> = [
            "commit: ",
            "file: ",
            "language: ",
            "symbols: ",
            "change: ",
            "added_lines:",
        ]
        .iter()
        .map(|needle| {
            body.find(needle)
                .unwrap_or_else(|| panic!("section {needle:?} missing in:\n{body}"))
        })
        .collect();
        for window in positions.windows(2) {
            assert!(window[0] < window[1], "sections out of order:\n{body}");
        }
    }

    #[test]
    fn build_skips_optional_sections_when_empty() {
        // No language, no symbols, no commit message — the builder
        // collapses to file + change + added_lines.
        let h = hunk("+x\n", None, ChangeKind::Modified);
        let body = build(&h, "", &[]);
        assert!(!body.contains("language:"));
        assert!(!body.contains("symbols:"));
        assert!(!body.contains("commit:"));
        assert!(body.contains("file: src/fetch.rs"));
        assert!(body.contains("change: modified"));
        assert!(body.contains("added_lines:\nx"));
    }

    #[test]
    fn extract_added_lines_strips_metadata_and_keeps_only_added_bodies() {
        // Plan 11 Task 2.1 Step 2: +++/---/hunk-header/deletion/context
        // all dropped; only +-prefixed content survives, with the
        // leading + removed.
        let raw =
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,3 @@\n-removed line\n unchanged\n+added one\n+added two\n";
        let added = extract_added_lines(raw);
        assert_eq!(added, "added one\nadded two");
    }

    #[test]
    fn build_returns_empty_when_diff_has_no_added_lines() {
        // Step 4 fallback contract: if the builder yields an empty body
        // for added_lines AND no metadata sections apply, callers
        // should fall back to the raw diff. Documented here so the
        // indexer's caller-side fallback in Task 2.1 Step 4 has a
        // contract to point at.
        let h = hunk(
            "--- a/x.rs\n+++ b/x.rs\n@@\n unchanged\n-deleted\n",
            None,
            ChangeKind::Deleted,
        );
        let body = build(&h, "", &[]);
        // Even with no added lines, the file + change sections are
        // still emitted — the result is non-empty but lacks the
        // added_lines section. Callers identify the "fall back to raw
        // diff" case by checking for the added_lines marker.
        assert!(!body.contains("added_lines:"));
        assert!(body.contains("change: deleted"));
    }
}
