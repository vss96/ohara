//! Plan 21: hydration helpers extracted from `explain/mod.rs`.
//!
//! These were private free functions in the monolithic `explain.rs`.
//! Extracting them lets the hydrator be unit-tested with a `MockStorage`
//! without running a real `Blamer::blame_range`.

use crate::diff_text::{truncate_diff, DIFF_EXCERPT_MAX_LINES};
use crate::explain::{ExplainHit, RelatedCommit};
use crate::storage::Storage;
use crate::types::{Provenance, RepoId};
use crate::Result;
use chrono::{DateTime, Utc};

/// Output of `hydrate_blame_results`. Mirrors the shape that
/// `explain_change` assembles from inline variables today; extracting
/// it into a named struct lets `explain/orchestrator.rs` compose the
/// final `(Vec<ExplainHit>, ExplainMeta)` without re-reading storage.
pub struct HydratedBlame {
    /// Enriched hits, ordered as they came from the blame ranges (sort
    /// to newest-first happens in the orchestrator after hydration so
    /// the orchestrator controls the `k` cap logic).
    pub hits: Vec<ExplainHit>,
    /// Fraction of blame-attributed lines that resolved to an indexed
    /// commit. 1.0 means full attribution; <1.0 means some SHAs were
    /// absent from the index.
    pub coverage: f32,
    /// Set when any lines were missed (file not found, unindexed SHAs).
    pub limitation: Option<String>,
    /// Set when the related-commit enrichment was constrained.
    pub enrichment_limitation: Option<String>,
    /// Contextual neighbours from `collect_related_commits`. Empty when
    /// `query.include_related` is false.
    pub related_commits: Vec<RelatedCommit>,
    /// Clamped line range derived from the blame output.
    pub clamped_range: (u32, u32),
}

/// Hydrate a pre-computed `Vec<BlameRange>` into `HydratedBlame`.
///
/// Deliberately does NOT call `BlameSource::blame_range` — callers
/// (the orchestrator or the engine's cache path) supply ranges that
/// were already computed. This is the seam that makes the BlameCache
/// wiring in Phase E possible: cached ranges bypass the blamer and go
/// straight here.
///
/// Uses `Storage::get_commits_by_sha` (Task B.1) to resolve all SHAs
/// in a single batched storage call.
pub async fn hydrate_blame_results(
    storage: &dyn Storage,
    blame_ranges: Vec<super::BlameRange>,
    query: &super::ExplainQuery,
    repo_id: &RepoId,
) -> Result<HydratedBlame> {
    // Derive the clamped range and line attribution totals from the blame
    // output — mirrors the existing logic in `explain_change`.
    let (clamped_start, clamped_end, lines_attributed_total) = if blame_ranges.is_empty() {
        (query.line_start, query.line_end, 0u32)
    } else {
        let mut min_line = u32::MAX;
        let mut max_line = 0u32;
        let mut total = 0u32;
        for r in &blame_ranges {
            for &l in &r.lines {
                if l < min_line {
                    min_line = l;
                }
                if l > max_line {
                    max_line = l;
                }
                total += 1;
            }
        }
        if min_line == u32::MAX {
            (query.line_start, query.line_end, 0)
        } else {
            (min_line, max_line, total)
        }
    };

    // Batch-resolve all unique SHAs in one storage round-trip.
    let shas: Vec<String> = blame_ranges.iter().map(|r| r.commit_sha.clone()).collect();
    let commit_map = storage.get_commits_by_sha(repo_id, &shas).await?;

    let mut hits: Vec<ExplainHit> = Vec::with_capacity(blame_ranges.len());
    let mut skipped_shas: Vec<String> = Vec::new();
    let mut lines_attributed_indexed: u32 = 0;

    for r in blame_ranges {
        match commit_map.get(&r.commit_sha) {
            Some(cm) => {
                lines_attributed_indexed += r.lines.len() as u32;
                let (excerpt, truncated) = if query.include_diff {
                    let hunks = storage
                        .get_hunks_for_file_in_commit(repo_id, &cm.commit_sha, &query.file)
                        .await?;
                    let joined: String = hunks
                        .iter()
                        .map(|h| h.diff_text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    truncate_diff(&joined, DIFF_EXCERPT_MAX_LINES)
                } else {
                    (String::new(), false)
                };
                let date = DateTime::<Utc>::from_timestamp(cm.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                hits.push(ExplainHit {
                    commit_sha: cm.commit_sha.clone(),
                    commit_message: cm.message.clone(),
                    commit_author: cm.author.clone(),
                    commit_date: date,
                    blame_lines: r.lines,
                    file_path: query.file.clone(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    provenance: Provenance::Exact,
                });
            }
            None => {
                tracing::debug!(
                    sha = %r.commit_sha,
                    "hydrate_blame_results: skipping unindexed commit"
                );
                skipped_shas.push(r.commit_sha);
            }
        }
    }

    let coverage = if lines_attributed_total == 0 {
        0.0
    } else {
        lines_attributed_indexed as f32 / lines_attributed_total as f32
    };
    let limitation = build_limitation(
        lines_attributed_total,
        &skipped_shas,
        clamped_start,
        clamped_end,
    );

    let (related_commits, enrichment_limitation) = if !query.include_related {
        (Vec::new(), None)
    } else if hits.is_empty() {
        (
            Vec::new(),
            Some("no indexed blame anchors — no contextual neighbours available".into()),
        )
    } else {
        collect_related_commits(storage, repo_id, &query.file, &hits).await?
    };

    Ok(HydratedBlame {
        hits,
        coverage,
        limitation,
        enrichment_limitation,
        related_commits,
        clamped_range: (clamped_start, clamped_end),
    })
}

/// Build the `_meta.limitation` string from blame statistics.
pub(crate) fn build_limitation(
    total: u32,
    skipped: &[String],
    clamped_start: u32,
    clamped_end: u32,
) -> Option<String> {
    if total == 0 {
        return Some(
            "blame returned no attributable lines \
             (file missing in HEAD or empty range)"
                .into(),
        );
    }
    if !skipped.is_empty() {
        let n = skipped.len();
        let preview: Vec<&str> = skipped.iter().take(3).map(String::as_str).collect();
        let suffix = if n > preview.len() {
            format!(" (+{} more)", n - preview.len())
        } else {
            String::new()
        };
        return Some(format!(
            "{n} commit(s) older than the local index watermark were skipped: \
             [{}]{}; range covered: {clamped_start}..={clamped_end}",
            preview.join(", "),
            suffix,
        ));
    }
    None
}

/// Plan 12 Task 3.2 logic, now living in `hydrator.rs`.
///
/// Collects contextual neighbours per blame anchor. Per-anchor limits
/// (2 before / 2 after) and overall dedup-by-sha keep the response
/// payload bounded. Returns `(related, enrichment_limitation)`.
pub(crate) async fn collect_related_commits(
    storage: &dyn Storage,
    repo_id: &RepoId,
    file: &str,
    hits: &[ExplainHit],
) -> Result<(Vec<RelatedCommit>, Option<String>)> {
    use std::collections::BTreeSet;
    const NEIGHBOURS_BEFORE: u8 = 2;
    const NEIGHBOURS_AFTER: u8 = 2;

    let anchor_shas: BTreeSet<&str> = hits.iter().map(|h| h.commit_sha.as_str()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<RelatedCommit> = Vec::new();

    for hit in hits {
        let neighbours = storage
            .get_neighboring_file_commits(
                repo_id,
                file,
                &hit.commit_sha,
                NEIGHBOURS_BEFORE,
                NEIGHBOURS_AFTER,
            )
            .await?;
        for (touched, cm) in neighbours {
            if anchor_shas.contains(cm.commit_sha.as_str()) {
                continue;
            }
            if !seen.insert(cm.commit_sha.clone()) {
                continue;
            }
            let date = DateTime::<Utc>::from_timestamp(cm.ts, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default();
            out.push(RelatedCommit {
                commit_sha: cm.commit_sha,
                commit_message: cm.message,
                commit_author: cm.author,
                commit_date: date,
                touched_hunks: touched,
                provenance: Provenance::Inferred,
            });
        }
    }
    Ok((out, None))
}
