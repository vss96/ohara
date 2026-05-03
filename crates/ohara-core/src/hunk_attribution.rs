//! Plan 11 — per-hunk symbol attribution.
//!
//! Decides which symbol(s) a hunk touched from its `diff_text`:
//!
//! 1. **`ExactSpan`** (preferred). Caller supplies the post-image
//!    file source. We parse the hunk's `@@ -A,B +C,D @@` headers to
//!    derive post-image line ranges, then intersect those against
//!    each parsed symbol's line span. Every intersecting symbol is
//!    emitted with `AttributionKind::ExactSpan`.
//!
//! 2. **`HunkHeader`** (fallback). When no source is available (or
//!    no exact-span symbol matched), git2's diff format includes the
//!    enclosing function/class signature on the `@@` line itself —
//!    `@@ -10,5 +10,7 @@ fn fetch(url: &str) -> String {`. We
//!    extract the symbol name from the trailing context with a
//!    light per-language heuristic and emit it with
//!    `AttributionKind::HunkHeader`.
//!
//! Both paths can be combined. The v0.7 indexer never writes
//! `AttributionKind::FileFallback` rows — see plan 11's "no broad
//! file fallback in the first pass" constraint.

use crate::types::{HunkSymbol, Symbol, SymbolKind};

/// Inputs for `attribute_hunk`. Kept as an explicit struct (rather
/// than a long argument list) because the indexer assembles them
/// from several different sources per call.
pub struct AttributionInputs<'a> {
    pub diff_text: &'a str,
    /// Atomic symbols extracted from the post-image source via
    /// `ohara_parse::extract_atomic_symbols`. `None` means the file
    /// source wasn't available — caller falls back to header-only
    /// attribution.
    pub symbols: Option<&'a [Symbol]>,
    /// Source the symbols' byte spans were computed against. Only
    /// used when `symbols` is `Some`. Required for byte->line
    /// translation via `ohara_parse::symbol_line_span`.
    pub source: Option<&'a str>,
}

/// Convert a byte range `(span_start..span_end)` into 1-based
/// `(line_start, line_end_inclusive)` against `source`. Mirrored
/// inside this crate (rather than calling `ohara-parse`) because
/// `ohara-core` is the dependency root — leaf crates implement its
/// traits, never the other way round.
fn byte_span_to_line_span(span_start: u32, span_end: u32, source: &str) -> (u32, u32) {
    let bytes = source.as_bytes();
    let start = (span_start as usize).min(bytes.len());
    let end = (span_end as usize).min(bytes.len());
    let line_at = |pos: usize| -> u32 {
        let counted = bytes[..pos].iter().filter(|&&b| b == b'\n').count();
        u32::try_from(counted + 1).unwrap_or(u32::MAX)
    };
    let line_start = line_at(start);
    let line_end = if end == 0 {
        line_start
    } else {
        line_at(end - 1)
    };
    (line_start, line_end.max(line_start))
}

/// Returns `(start_line, line_count)` for every `@@ -A,B +C,D @@`
/// hunk header found in `diff_text`. Lines are 1-based per git's
/// convention. Lines without a count default to 1.
pub fn parse_post_image_ranges(diff_text: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        // Format: `@@ -A,B +C,D @@ <optional context>`.
        // Split on whitespace; the third whitespace-separated token
        // is the `+C,D` (or `+C`) range.
        let mut tokens = line.split_whitespace();
        // Skip the leading "@@" + the "-A,B" range.
        let _ = tokens.next();
        let _ = tokens.next();
        let plus = match tokens.next() {
            Some(t) if t.starts_with('+') => &t[1..],
            _ => continue,
        };
        let (start, count) = match plus.split_once(',') {
            Some((s, c)) => (s.parse().unwrap_or(0_u32), c.parse().unwrap_or(0_u32)),
            None => (plus.parse().unwrap_or(0_u32), 1_u32),
        };
        if start == 0 || count == 0 {
            continue;
        }
        out.push((start, count));
    }
    out
}

/// Extract the trailing-context portion of every `@@` line — git
/// puts the enclosing function/class signature there:
/// `@@ -10,5 +10,7 @@ fn fetch(url: &str) -> String {`.
///
/// Returns the suffix string (everything after the second `@@`),
/// trimmed. Empty / missing suffixes are skipped.
pub fn parse_hunk_header_suffixes(diff_text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        // Suffix is everything after the closing `@@`.
        if let Some(after) = line.splitn(3, "@@").nth(2) {
            let s = after.trim();
            if !s.is_empty() {
                out.push(s);
            }
        }
    }
    out
}

/// Pull a probable symbol name out of a hunk-header suffix using a
/// light per-language heuristic. Looks for the keyword that
/// introduces a definition (`fn`, `def`, `class`, `function`, etc.)
/// and returns the identifier that follows.
///
/// Returns `(name, kind)` so callers can populate the `HunkSymbol`'s
/// `kind` field instead of guessing. Returns `None` when no keyword
/// matched (e.g. the suffix is a comment, an import, or an arbitrary
/// expression).
pub fn parse_symbol_from_header_suffix(suffix: &str) -> Option<(String, SymbolKind)> {
    // Order matters when one keyword is a prefix of another (none
    // here, but kept stable). `class`/`struct`/`enum`/`interface` ->
    // Class; `fn`/`def`/`function` -> Function; `const`/`static` ->
    // Const. Method vs Function distinction needs source context to
    // tell apart — keep it as Function in the fallback path.
    const KEYWORDS: &[(&str, SymbolKind)] = &[
        ("fn", SymbolKind::Function),
        ("def", SymbolKind::Function),
        ("function", SymbolKind::Function),
        ("class", SymbolKind::Class),
        ("struct", SymbolKind::Class),
        ("enum", SymbolKind::Class),
        ("interface", SymbolKind::Class),
        ("trait", SymbolKind::Class),
        ("object", SymbolKind::Class),
        ("const", SymbolKind::Const),
    ];
    for (keyword, kind) in KEYWORDS {
        // Match keyword followed by whitespace and then an
        // identifier. Don't match prefixes (e.g. `function_x`) by
        // checking for whitespace boundary.
        let needle = format!("{keyword} ");
        let position = suffix
            .match_indices(needle.as_str())
            .find_map(|(i, _)| {
                // Make sure it's at start-of-string or preceded by
                // whitespace / `(` / `,` so we don't catch `_fn `.
                if i == 0 || matches!(suffix.as_bytes().get(i - 1), Some(b) if !b.is_ascii_alphanumeric() && *b != b'_')
                {
                    Some(i + needle.len())
                } else {
                    None
                }
            });
        if let Some(pos) = position {
            let rest = &suffix[pos..];
            // Take chars up to the first non-identifier byte.
            let end = rest
                .char_indices()
                .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            if end > 0 {
                return Some((rest[..end].to_string(), *kind));
            }
        }
    }
    None
}

/// Run the attribution pipeline against `inputs` and return one
/// `HunkSymbol` per attributed symbol. Deduplicates by `(kind, name)`
/// — if both ExactSpan and HunkHeader name the same symbol, the
/// ExactSpan record wins.
pub fn attribute_hunk(inputs: &AttributionInputs<'_>) -> Vec<HunkSymbol> {
    use crate::types::AttributionKind;
    use std::collections::BTreeMap;

    let mut out: BTreeMap<(SymbolKind, String), HunkSymbol> = BTreeMap::new();
    let ranges = parse_post_image_ranges(inputs.diff_text);

    // 1. ExactSpan path.
    if let (Some(symbols), Some(source)) = (inputs.symbols, inputs.source) {
        for symbol in symbols {
            let (sym_start, sym_end) =
                byte_span_to_line_span(symbol.span_start, symbol.span_end, source);
            for (range_start, range_count) in &ranges {
                let range_end = range_start.saturating_add(*range_count).saturating_sub(1);
                // Ranges intersect when sym_start <= range_end &&
                // range_start <= sym_end.
                if sym_start <= range_end && *range_start <= sym_end {
                    out.insert(
                        (symbol.kind, symbol.name.clone()),
                        HunkSymbol {
                            kind: symbol.kind,
                            name: symbol.name.clone(),
                            qualified_name: symbol.qualified_name.clone(),
                            attribution: AttributionKind::ExactSpan,
                        },
                    );
                    break;
                }
            }
        }
    }

    // 2. HunkHeader fallback. Only attaches if the same `(kind, name)`
    // wasn't already covered by an ExactSpan match.
    for suffix in parse_hunk_header_suffixes(inputs.diff_text) {
        if let Some((name, kind)) = parse_symbol_from_header_suffix(suffix) {
            out.entry((kind, name.clone())).or_insert(HunkSymbol {
                kind,
                name,
                qualified_name: None,
                attribution: AttributionKind::HunkHeader,
            });
        }
    }

    out.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AttributionKind;

    #[test]
    fn parse_post_image_ranges_extracts_start_and_count() {
        let diff = "@@ -1,3 +1,5 @@\n+a\n+b\n@@ -10,2 +12,4 @@ ctx\n+c\n";
        assert_eq!(parse_post_image_ranges(diff), vec![(1, 5), (12, 4)]);
    }

    #[test]
    fn parse_post_image_ranges_handles_single_line_form() {
        // git omits the count when it's 1.
        let diff = "@@ -5 +5 @@\n-x\n+y\n";
        assert_eq!(parse_post_image_ranges(diff), vec![(5, 1)]);
    }

    #[test]
    fn parse_hunk_header_suffixes_extracts_trailing_context() {
        let diff = "\
@@ -10,5 +10,7 @@ fn fetch(url: &str) -> String {
+    retry();
@@ -42,3 +44,3 @@ class UserController {
+    pass
";
        assert_eq!(
            parse_hunk_header_suffixes(diff),
            vec!["fn fetch(url: &str) -> String {", "class UserController {"],
        );
    }

    #[test]
    fn parse_symbol_from_header_suffix_handles_common_languages() {
        for (suffix, expected_name, expected_kind) in [
            (
                "fn fetch(url: &str) -> String {",
                "fetch",
                SymbolKind::Function,
            ),
            ("def load_config():", "load_config", SymbolKind::Function),
            (
                "class UserController {",
                "UserController",
                SymbolKind::Class,
            ),
            ("public class Foo {", "Foo", SymbolKind::Class),
            ("trait Storage: Send {", "Storage", SymbolKind::Class),
            (
                "const MAX_RETRIES: u8 = 3;",
                "MAX_RETRIES",
                SymbolKind::Const,
            ),
        ] {
            let got = parse_symbol_from_header_suffix(suffix)
                .unwrap_or_else(|| panic!("no match in suffix: {suffix:?}"));
            assert_eq!(got.0, expected_name, "name mismatch for {suffix:?}");
            assert_eq!(got.1, expected_kind, "kind mismatch for {suffix:?}");
        }
    }

    #[test]
    fn parse_symbol_from_header_suffix_rejects_garbage() {
        // Comment-only context, arbitrary expression — no keyword match.
        assert!(parse_symbol_from_header_suffix("// pure context line").is_none());
        assert!(parse_symbol_from_header_suffix("    let x = 5;").is_none());
    }

    #[test]
    fn attribute_hunk_with_no_inputs_returns_empty() {
        let inputs = AttributionInputs {
            diff_text: "+just text without headers\n",
            symbols: None,
            source: None,
        };
        assert!(attribute_hunk(&inputs).is_empty());
    }

    #[test]
    fn attribute_hunk_emits_hunk_header_when_no_source_available() {
        // Plan 11 Task 3.1 Step 3: HunkHeader fallback fires when the
        // parser couldn't reach the file but git's @@-line context
        // still names an enclosing function.
        let inputs = AttributionInputs {
            diff_text: "@@ -5,3 +5,5 @@ fn retry_with_backoff() {\n+    sleep(d);\n",
            symbols: None,
            source: None,
        };
        let attrs = attribute_hunk(&inputs);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "retry_with_backoff");
        assert_eq!(attrs[0].attribution, AttributionKind::HunkHeader);
    }

    fn fake_symbol(name: &str, span_start: u32, span_end: u32) -> Symbol {
        Symbol {
            file_path: "a.rs".into(),
            language: "rust".into(),
            kind: SymbolKind::Function,
            name: name.into(),
            qualified_name: None,
            sibling_names: Vec::new(),
            span_start,
            span_end,
            blob_sha: "sha".into(),
            source_text: String::new(),
        }
    }

    #[test]
    fn attribute_hunk_emits_exact_span_when_symbols_intersect_post_image_range() {
        // Plan 11 Task 3.1 Step 2: ExactSpan attribution from a hunk's
        // post-image line range intersecting a parsed symbol's span.
        // Three side-by-side functions on lines 1, 2, 3; the diff
        // modifies line 2. Only `bravo` should be attributed.
        let source = "fn alpha() {}\nfn bravo() { /*body*/ }\nfn charlie() {}\n";
        // Byte spans pre-computed so we don't depend on ohara-parse
        // (which would create a circular dep — ohara-parse depends
        // on ohara-core, not vice versa).
        let alpha_end = (source.find('\n').unwrap()) as u32;
        let line2_start = alpha_end + 1;
        let line2_end = source[line2_start as usize..]
            .find('\n')
            .map(|i| line2_start + i as u32)
            .unwrap();
        let line3_start = line2_end + 1;
        let line3_end = source[line3_start as usize..]
            .find('\n')
            .map(|i| line3_start + i as u32)
            .unwrap();
        let symbols = vec![
            fake_symbol("alpha", 0, alpha_end),
            fake_symbol("bravo", line2_start, line2_end),
            fake_symbol("charlie", line3_start, line3_end),
        ];
        let diff = "@@ -2,1 +2,1 @@\n-fn bravo() { /*body*/ }\n+fn bravo() { /*new body*/ }\n";
        let inputs = AttributionInputs {
            diff_text: diff,
            symbols: Some(&symbols),
            source: Some(source),
        };
        let attrs = attribute_hunk(&inputs);
        let names: Vec<String> = attrs.iter().map(|a| a.name.clone()).collect();
        assert_eq!(names, vec!["bravo".to_string()]);
        assert_eq!(attrs[0].attribution, AttributionKind::ExactSpan);
    }

    #[test]
    fn attribute_hunk_prefers_exact_span_over_header_when_both_match_same_symbol() {
        let source = "fn fetch() { /*body*/ }\n";
        let end = (source.len() - 1) as u32; // exclude trailing newline
        let symbols = vec![fake_symbol("fetch", 0, end)];
        let diff =
            "@@ -1,1 +1,1 @@ fn fetch() {\n-fn fetch() { /*body*/ }\n+fn fetch() { /*new*/ }\n";
        let inputs = AttributionInputs {
            diff_text: diff,
            symbols: Some(&symbols),
            source: Some(source),
        };
        let attrs = attribute_hunk(&inputs);
        assert_eq!(attrs.len(), 1, "exact-span should dedupe the header match");
        assert_eq!(attrs[0].attribution, AttributionKind::ExactSpan);
    }
}
