//! `explain_change` MCP tool — given a file + line range, return the
//! commits that introduced and shaped that code, ordered newest-first.
//!
//! Plan 5 / Task 8. Companion to `find_pattern`: where `find_pattern`
//! answers "how was X done before?", this tool answers "why does THIS
//! code look the way it does?". Determined by `git blame`, not
//! embeddings — every result has `provenance = "EXACT"`.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

pub const TOOL_DESCRIPTION: &str = "\
Explain why specific lines of code look the way they do, by walking
git history and returning the commits that introduced and shaped them.

USE WHEN the user:
  - asks \"why does this code look this way\" / \"how did this get here\"
  - wants \"git archaeology\" / \"who wrote this\" / \"blame this\"
  - wants the history of a specific block, function, or line range

DO NOT USE for:
  - searching for similar past patterns — use `find_pattern` instead
  - inspecting current code — use Grep/Read for that
  - general programming questions

Returns: newest-first commits with messages, authors, dates, the
specific blame_lines they own, diff excerpts, and
provenance = \"EXACT\" (git blame is git-truth).";

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ExplainChangeInput {
    /// Repo-relative file path (e.g. `src/auth.rs`).
    pub file: String,
    /// 1-based start line, inclusive. Defaults to 1.
    #[serde(default = "default_line_start")]
    pub line_start: u32,
    /// 1-based end line, inclusive. Defaults to 0 — the tool then
    /// resolves "0" to the file's actual last line on the server side
    /// (so the input default doesn't have to read the file).
    #[serde(default = "default_line_end")]
    pub line_end: u32,
    /// Number of commits to return (1..=20). Defaults to 5.
    #[serde(default = "default_k")]
    pub k: u8,
    /// Include diff excerpts in each hit. Defaults to true.
    #[serde(default = "default_include_diff")]
    pub include_diff: bool,
}

fn default_line_start() -> u32 {
    1
}

/// Sentinel: "use end-of-file". The server resolves to the actual last
/// line by reading the workdir file; passing 0 here keeps the schema
/// simple and JSON-friendly.
fn default_line_end() -> u32 {
    0
}

fn default_k() -> u8 {
    5
}

fn default_include_diff() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_change_input_parses_default_k_and_lines() {
        // Plan 5 / Task 8.r: an MCP client may send only `{ "file": ".." }`
        // — every other field has a default. The defaults must be:
        // line_start=1, line_end=0 (end-of-file sentinel), k=5,
        // include_diff=true.
        let json = r#"{ "file": "src/auth.rs" }"#;
        let parsed: ExplainChangeInput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.file, "src/auth.rs");
        assert_eq!(parsed.line_start, 1);
        assert_eq!(parsed.line_end, 0);
        assert_eq!(parsed.k, 5);
        assert!(parsed.include_diff);
    }

    #[test]
    fn explain_change_input_round_trips_explicit_values() {
        let json = r#"{
            "file": "src/x.rs",
            "line_start": 10,
            "line_end": 42,
            "k": 3,
            "include_diff": false
        }"#;
        let parsed: ExplainChangeInput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.line_start, 10);
        assert_eq!(parsed.line_end, 42);
        assert_eq!(parsed.k, 3);
        assert!(!parsed.include_diff);
    }
}
