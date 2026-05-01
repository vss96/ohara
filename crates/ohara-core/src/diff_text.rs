//! Shared diff-text helpers used by retrieval, explain, blame, and CLI.
//!
//! These utilities operate on raw diff / file text and have no dependency
//! on git or storage, so they live here in core where every higher layer
//! can reach them. Centralizing them keeps the truncation cap and the
//! line-counting semantics consistent across MCP tools and the CLI.

/// Per-line diff truncation cap for `*_excerpt` fields surfaced through
/// MCP. Tools that produce diff excerpts (`find_pattern`,
/// `explain_change`) should pass this constant to `truncate_diff` so the
/// payloads look consistent across surfaces.
pub const DIFF_EXCERPT_MAX_LINES: usize = 80;

/// Truncate `s` to at most `max_lines` lines.
///
/// Returns `(excerpt, truncated)`. When the input fits, the excerpt is
/// returned unchanged and `truncated` is `false`. When it doesn't, the
/// excerpt contains the first `max_lines` lines followed by a
/// `... (N more lines)\n` marker and `truncated` is `true`.
///
/// A trailing partial line (input not ending with `\n`) counts as one
/// line.
pub fn truncate_diff(s: &str, max_lines: usize) -> (String, bool) {
    let nl = s.bytes().filter(|&b| b == b'\n').count();
    let has_trailing_partial = !s.is_empty() && !s.ends_with('\n');
    let total_lines = nl + if has_trailing_partial { 1 } else { 0 };

    if total_lines <= max_lines {
        return (s.to_string(), false);
    }

    let mut end = 0;
    let mut count = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == max_lines {
                end = i + 1;
                break;
            }
        }
    }

    let extra = total_lines - max_lines;
    let mut out = s[..end].to_string();
    out.push_str(&format!("... ({} more lines)\n", extra));
    (out, true)
}

/// Count the lines in `s`. An empty string is `0` lines; a non-empty
/// string with no trailing newline counts the trailing partial as a
/// line.
pub fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|&b| b == b'\n').count() as u32;
    if s.ends_with('\n') {
        nl
    } else {
        nl + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_diff_passes_through_short_input() {
        let (out, truncated) = truncate_diff("a\nb\nc\n", 5);
        assert_eq!(out, "a\nb\nc\n");
        assert!(!truncated);
    }

    #[test]
    fn truncate_diff_caps_long_input_with_marker() {
        let input = "1\n2\n3\n4\n5\n";
        let (out, truncated) = truncate_diff(input, 2);
        assert!(truncated);
        assert_eq!(out, "1\n2\n... (3 more lines)\n");
    }

    #[test]
    fn truncate_diff_treats_trailing_partial_as_line() {
        let input = "1\n2\n3"; // 3 lines total
        let (out, truncated) = truncate_diff(input, 2);
        assert!(truncated);
        assert_eq!(out, "1\n2\n... (1 more lines)\n");
    }

    #[test]
    fn count_lines_handles_empty_and_trailing_partial() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
    }
}
