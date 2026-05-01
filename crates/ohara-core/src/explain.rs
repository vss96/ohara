//! `explain_change` orchestrator — given a file + line range, return the
//! commits that introduced and shaped that code, ordered newest-first.
//!
//! Plan 5 / Track A. Counterpart to `find_pattern`: where the retriever
//! answers "how was X done before" via embeddings + BM25 + rerank, this
//! module answers "why does THIS code look the way it does" via
//! deterministic `git blame`. No embeddings, no rerank — every result has
//! `provenance = EXACT`.
//!
//! `ohara-core` stays git-free: the `BlameSource` trait abstracts over
//! `git2::Repository::blame_file`, with the real implementation living in
//! `ohara-git::Blamer`.

use crate::types::Provenance;
use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One commit's contribution to a blame query, with the lines (within
/// the queried range) it owns. Returned by `BlameSource::blame_range`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameRange {
    pub commit_sha: String,
    /// 1-based line numbers within the queried range; sorted ascending.
    pub lines: Vec<u32>,
}

/// Abstraction over `git2::Repository::blame_file`. Implemented by
/// `ohara-git::Blamer`. Mirrors the `CommitSource` / `SymbolSource`
/// pattern — keeps `ohara-core` git-free.
#[async_trait]
pub trait BlameSource: Send + Sync {
    /// Blame a contiguous, 1-based, inclusive line range. The
    /// implementation may clamp `line_end` to the file's actual length.
    /// Returns one entry per distinct commit-of-origin; lines are the
    /// in-range subset that commit owns.
    async fn blame_range(
        &self,
        file: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<Vec<BlameRange>>;
}

/// Caller's request to `explain_change`. `line_start` / `line_end` are
/// 1-based, inclusive; the orchestrator clamps to file length and
/// returns a `_meta.limitation` when the range is degenerate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainQuery {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Number of commits to return (1..=20). Caller-side clamp is
    /// enforced inside `explain_change` itself.
    pub k: u8,
    /// When false, `ExplainHit::diff_excerpt` is left empty so the
    /// caller (e.g. the MCP layer) can render a tighter response. The
    /// orchestrator still computes `blame_lines` either way.
    pub include_diff: bool,
}

/// One commit's contribution, enriched for display. Recency order
/// (commit timestamp desc) is the orchestrator's contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainHit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    /// ISO-8601 / RFC-3339, derived from the commit's unix timestamp.
    pub commit_date: String,
    /// 1-based queried-range line numbers this commit owns; sorted asc.
    pub blame_lines: Vec<u32>,
    pub file_path: String,
    pub diff_excerpt: String,
    pub diff_truncated: bool,
    /// Always `Provenance::Exact` — git blame is git-truth, not
    /// inferred. Serializes to `"EXACT"`.
    pub provenance: Provenance,
}

/// Diagnostic envelope returned alongside hits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainMeta {
    /// `(line_start, line_end)` after clamping to file bounds.
    pub lines_queried: (u32, u32),
    /// Number of distinct commits in the post-skip result set (matches
    /// `hits.len()` once the orchestrator caps to `k`).
    pub commits_unique: usize,
    /// Fraction of queried lines that resolved to an indexed commit.
    /// 1.0 means every line was attributed; less than 1.0 means at
    /// least one line landed on a SHA the local index doesn't know.
    pub blame_coverage: f32,
    /// Free-form note when the result set is constrained (e.g.
    /// "file was renamed; pre-rename history not reached", or
    /// "file does not exist in HEAD").
    pub limitation: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doc-test-style sanity check that the trait surface compiles
    /// against a hand-rolled fake. The orchestrator's behavioural tests
    /// land in Task 7 once `explain_change` exists.
    struct FakeBlamer;

    #[async_trait]
    impl BlameSource for FakeBlamer {
        async fn blame_range(
            &self,
            _file: &str,
            _line_start: u32,
            _line_end: u32,
        ) -> Result<Vec<BlameRange>> {
            Ok(vec![BlameRange {
                commit_sha: "abc".into(),
                lines: vec![1, 2, 3],
            }])
        }
    }

    #[tokio::test]
    async fn blame_source_trait_object_round_trips_a_fake() {
        let b: &dyn BlameSource = &FakeBlamer;
        let out = b.blame_range("any.rs", 1, 3).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].commit_sha, "abc");
        assert_eq!(out[0].lines, vec![1, 2, 3]);
    }
}
