//! Helpers for adapters that parse line-delimited JSON agent output.

use std::path::Path;

/// Scan `text` from the last line backwards and return the first line that
/// parses as JSON and for which `pick` returns `Some`.
pub fn last_json_line<T>(text: &str, pick: impl Fn(&serde_json::Value) -> Option<T>) -> Option<T> {
    text.lines().rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        pick(&v)
    })
}

/// Like [`last_json_line`], over a whole file. The capped `stdout_tail` can
/// cut an oversized final event; the transcript holds the complete output.
/// A missing or unreadable file yields `None`.
pub async fn last_json_line_in_file<T>(
    path: &Path,
    pick: impl Fn(&serde_json::Value) -> Option<T>,
) -> Option<T> {
    let bytes = tokio::fs::read(path).await.ok()?;
    last_json_line(&String::from_utf8_lossy(&bytes), pick)
}
