//! Pure ranking math for the retrieval pipeline.
//!
//! Two free functions sit between the I/O of the coordinator and the I/O
//! of the cross-encoder reranker:
//!
//! - [`fuse_to_pool`] — Reciprocal Rank Fusion over per-lane rankings,
//!   truncated to the rerank pool size. Materializes [`HunkHit`]s from
//!   a caller-owned id-to-hit lookup.
//! - [`apply_recency`] — exp-decay recency multiplier; writes the
//!   combined score back into [`HunkHit::similarity`] and re-sorts.
//!
//! Both are sync, pure data transformations. Tests don't need
//! `tokio::test` and don't need fakes for storage / embedder / reranker.

use crate::query::reciprocal_rank_fusion;
use crate::storage::{HunkHit, HunkId};
use std::collections::HashMap;

/// RRF-merge per-lane id rankings, truncate to `pool_k`, and materialize
/// [`HunkHit`]s by looking each id up in `by_id`. Ids missing from the
/// lookup (a programming error in the caller) are silently dropped —
/// the same behavior the coordinator had before extraction.
///
/// `rrf_k` is the smoothing constant; see [`reciprocal_rank_fusion`].
pub fn fuse_to_pool(
    rankings: &[Vec<HunkId>],
    by_id: &HashMap<HunkId, HunkHit>,
    rrf_k: u32,
    pool_k: usize,
) -> Vec<HunkHit> {
    let fused = reciprocal_rank_fusion(rankings, rrf_k);
    fused
        .into_iter()
        .take(pool_k)
        .filter_map(|id| by_id.get(&id).cloned())
        .collect()
}

/// Apply the exp-decay recency multiplier:
/// `final = similarity * (1.0 + recency_weight * exp(-age_days / half_life_days))`.
///
/// Writes the combined score back into [`HunkHit::similarity`] and
/// re-sorts highest-first. Empty input is returned unchanged. The
/// `profile.recency_multiplier` nudge (plan 12) is the caller's
/// responsibility — fold it into `recency_weight` before calling.
pub fn apply_recency(
    hits: Vec<HunkHit>,
    recency_weight: f32,
    recency_half_life_days: f32,
    now_unix: i64,
) -> Vec<HunkHit> {
    if hits.is_empty() {
        return hits;
    }
    let mut scored: Vec<(HunkHit, f32)> = hits
        .into_iter()
        .map(|mut h| {
            let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86_400.0;
            let recency = (-age_days / recency_half_life_days).exp();
            let combined = h.similarity * (1.0 + recency_weight * recency);
            h.similarity = combined;
            (h, combined)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(h, _)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangeKind, CommitMeta, Hunk};

    fn make_hit(id: HunkId, ts: i64, sim: f32) -> HunkHit {
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "x".into(),
                file_path: "f.rs".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: format!("diff-{id}"),
            },
            commit: CommitMeta {
                commit_sha: "x".into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts,
                message: "m".into(),
            },
            similarity: sim,
        }
    }

    #[test]
    fn fuse_to_pool_orders_two_lane_ids_above_single_lane() {
        let h1 = make_hit(1, 0, 0.9);
        let h2 = make_hit(2, 0, 0.5);
        let h3 = make_hit(3, 0, 0.4);
        let rankings = vec![
            vec![1, 2], // lane A
            vec![2, 1], // lane B
            vec![3],    // lane C
        ];
        let by_id: HashMap<HunkId, HunkHit> =
            [h1, h2, h3].into_iter().map(|h| (h.hunk_id, h)).collect();
        let out = fuse_to_pool(&rankings, &by_id, 60, 10);
        assert_eq!(out.len(), 3, "all three unique ids survive rrf");
        assert!(
            out.iter().position(|h| h.hunk_id == 3).unwrap() > 0,
            "id=3 (single-lane) must rank below the two-lane ids"
        );
    }

    #[test]
    fn fuse_to_pool_truncates_to_pool_k() {
        let by_id: HashMap<HunkId, HunkHit> =
            (1..=5).map(|id| (id, make_hit(id, 0, 0.5))).collect();
        let rankings = vec![vec![1, 2, 3, 4, 5]];
        let out = fuse_to_pool(&rankings, &by_id, 60, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].hunk_id, 1);
        assert_eq!(out[1].hunk_id, 2);
    }

    #[test]
    fn fuse_to_pool_empty_rankings_returns_empty() {
        let by_id: HashMap<HunkId, HunkHit> = HashMap::new();
        let out = fuse_to_pool(&[], &by_id, 60, 10);
        assert!(out.is_empty());
    }

    #[test]
    fn apply_recency_newer_hit_ranks_higher() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(2, now - 100 * day, 1.0), // older
            make_hit(1, now - day, 1.0),       // newer
        ];
        let out = apply_recency(hits, 0.05, 90.0, now);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].hunk_id, 1,
            "newer hit (1 day old) must outrank older (100 days old)"
        );
        assert_eq!(out[1].hunk_id, 2);
    }

    #[test]
    fn apply_recency_zero_weight_preserves_input_order() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(10, now - day, 1.0),
            make_hit(11, now - 200 * day, 1.0),
        ];
        let out = apply_recency(hits, 0.0, 90.0, now);
        assert_eq!(out[0].hunk_id, 10);
        assert_eq!(out[1].hunk_id, 11);
    }

    #[test]
    fn apply_recency_writes_combined_score_into_similarity() {
        // Plan 22 contract: the post-recency score lands in
        // `similarity` so downstream consumers (PatternHit::combined_score
        // mapping in retriever/mod.rs) see the actual rank base.
        let now = 1_700_000_000_i64;
        let hits = vec![make_hit(7, now, 0.5)];
        let out = apply_recency(hits, 0.05, 90.0, now);
        assert_eq!(out.len(), 1);
        // age = 0 → recency factor = 1.0 → combined = 0.5 * (1 + 0.05) = 0.525
        assert!((out[0].similarity - 0.525).abs() < 1e-5);
    }

    #[test]
    fn apply_recency_empty_input_returns_empty() {
        let out = apply_recency(vec![], 0.05, 90.0, 1_700_000_000);
        assert!(out.is_empty());
    }
}
