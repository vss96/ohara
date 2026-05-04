//! `explain_change` orchestrator — given a file + line range, return the
//! commits that introduced and shaped that code, ordered newest-first.
//!
//! Counterpart to `find_pattern`: where the retriever answers "how was X
//! done before" via embeddings + BM25 + rerank, this module answers "why
//! does THIS code look the way it does" via deterministic `git blame`.
//! No embeddings, no rerank — every result has `provenance = EXACT`.
//!
//! `ohara-core` stays git-free: the `BlameSource` trait abstracts over
//! `git2::Repository::blame_file`, with the real implementation living in
//! `ohara-git::Blamer`.

pub(crate) mod hydrator;

use crate::perf_trace::timed_phase;
use crate::storage::Storage;
use crate::types::{Provenance, RepoId};
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
    /// Plan 12 Task 3.2: when true, the orchestrator attaches
    /// contextual commits that touched the same file around each
    /// blame anchor (see `ExplainMeta::related_commits`). Default
    /// true — clients that don't want the extra payload can flip
    /// this off.
    #[serde(default = "default_true")]
    pub include_related: bool,
}

fn default_true() -> bool {
    true
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

/// `k` clamp matches the spec: 1..=20, default 5 enforced at the caller.
const K_MAX: u8 = 20;

/// One contextual commit added by the plan-12 explain enrichment.
/// Distinct from `ExplainHit` (which carries blame-exact provenance)
/// because related commits are file-scope context, NOT line-level
/// proof. Clients should display them as "what nearby changes shaped
/// this area" rather than "which commits introduced these lines".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedCommit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    pub commit_date: String,
    /// Number of hunks this commit produced for the queried file.
    pub touched_hunks: u32,
    /// Always `Provenance::Inferred` — file-scoped neighbour, not
    /// line-level blame evidence.
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
    /// Plan 12 Task 3.2: contextual commits that touched the same
    /// file near the blame anchors. NOT line-level proof — clients
    /// should display these as "what nearby changes shaped this
    /// area", not "which commits introduced these lines". Empty
    /// when `ExplainQuery.include_related` is false or no
    /// neighbouring commits exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_commits: Vec<RelatedCommit>,
    /// Plan 12 Task 3.2: free-form note when the enrichment was
    /// constrained (e.g. "anchor not indexed; no neighbours
    /// returned").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrichment_limitation: Option<String>,
}

/// Run an `explain_change` query end-to-end.
///
/// 1. Ask the `BlameSource` for line ownership over the queried range.
/// 2. Delegate all storage hydration to `hydrator::hydrate_blame_results`.
/// 3. Sort hits newest-first by `commit_date`, cap to `query.k`.
/// 4. Assemble and return `(Vec<ExplainHit>, ExplainMeta)`.
///
/// The BlameCache wiring (skipping step 1 on a cache hit) lives in
/// `ohara_engine::engine::explain_change` — the core orchestrator
/// always runs the blamer; it is the engine's responsibility to short-
/// circuit when cached ranges are available.
pub async fn explain_change(
    storage: &dyn Storage,
    blamer: &dyn BlameSource,
    repo_id: &RepoId,
    query: &ExplainQuery,
) -> Result<(Vec<ExplainHit>, ExplainMeta)> {
    let raw_blame = timed_phase(
        "blame",
        blamer.blame_range(&query.file, query.line_start, query.line_end),
    )
    .await?;

    let hydrated = timed_phase(
        "hydrate_explain",
        hydrator::hydrate_blame_results(storage, raw_blame, query, repo_id),
    )
    .await?;

    let mut hits = hydrated.hits;
    hits.sort_by(|a, b| match b.commit_date.cmp(&a.commit_date) {
        std::cmp::Ordering::Equal => a.commit_sha.cmp(&b.commit_sha),
        other => other,
    });
    let k = query.k.clamp(1, K_MAX) as usize;
    hits.truncate(k);

    let meta = ExplainMeta {
        lines_queried: hydrated.clamped_range,
        commits_unique: hits.len(),
        blame_coverage: hydrated.coverage,
        limitation: hydrated.limitation,
        related_commits: hydrated.related_commits,
        enrichment_limitation: hydrated.enrichment_limitation,
    };
    Ok((hits, meta))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

