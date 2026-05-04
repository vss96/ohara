//! Unit tests for `diff_text::truncate_diff`.

use crate::diff_text::truncate_diff;

#[test]
fn truncate_marks_truncation_for_long_diffs() {
    let big = (0..200)
        .map(|i| format!("line {}\n", i))
        .collect::<String>();
    let (out, trunc) = truncate_diff(&big, 80);
    assert!(trunc);
    assert!(out.contains("more lines"));
}

#[test]
fn truncate_passthrough_for_short_diffs() {
    let small = "line a\nline b\n";
    let (out, trunc) = truncate_diff(small, 80);
    assert!(!trunc);
    assert_eq!(out, small);
}

#[test]
fn truncate_does_not_pad_at_exact_boundary() {
    let exact = "a\nb\nc\n";
    let (out, trunc) = truncate_diff(exact, 3);
    assert!(!trunc);
    assert_eq!(out, exact);
}

#[test]
fn truncate_counts_trailing_partial_line() {
    let with_partial = "a\nb\nc\nd";
    let (out, trunc) = truncate_diff(with_partial, 3);
    assert!(trunc);
    assert!(out.contains("(1 more lines)"));
    assert!(out.starts_with("a\nb\nc\n"));
}
