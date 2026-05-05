//! Plan 20 — recency multiplier refiner.
//!
//! Applies the half-life exp-decay recency factor to each hit's
//! `similarity` score and re-sorts. This is the same formula used
//! in the pre-plan-20 inline `find_pattern_with_profile`:
//!
//! ```text
//! final = similarity * (1.0 + recency_weight * exp(-age_days / half_life_days))
//! ```
//!
//! The `profile.recency_multiplier` nudge (plan 12) is applied by the
//! coordinator before constructing this refiner: it multiplies
//! `RankingWeights::recency_weight` by `profile.recency_multiplier`
//! and passes the result in `RankingWeights::recency_weight`.

use super::ScoreRefiner;
use crate::retriever::RankingWeights;
use crate::storage::HunkHit;
use async_trait::async_trait;

pub struct RecencyRefiner {
    weights: RankingWeights,
    now_unix: i64,
}

impl RecencyRefiner {
    /// Construct with weights and the current Unix timestamp.
    /// The coordinator passes `now_unix` from the outer call so all
    /// hits in one pipeline run are ranked against the same instant.
    pub fn new(weights: RankingWeights, now_unix: i64) -> Self {
        Self { weights, now_unix }
    }
}

#[async_trait]
impl ScoreRefiner for RecencyRefiner {
    async fn refine(&self, _query_text: &str, hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        let mut scored: Vec<(HunkHit, f32)> = hits
            .into_iter()
            .map(|mut h| {
                let age_days = ((self.now_unix - h.commit.ts).max(0) as f32) / 86_400.0;
                let recency = (-age_days / self.weights.recency_half_life_days).exp();
                let combined = h.similarity * (1.0 + self.weights.recency_weight * recency);
                // Plan 22: write the combined score back into
                // `similarity` so downstream consumers (mod.rs maps it
                // into `PatternHit::combined_score`) report the actual
                // post-recency rank base — not the raw rerank input.
                // The retriever's contract since plan 20 is "the
                // coordinator already applied recency"; this line
                // makes that comment true.
                h.similarity = combined;
                (h, combined)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().map(|(h, _)| h).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::RankingWeights;
    use crate::storage::{HunkHit, HunkId};

    fn make_hit(id: HunkId, ts: i64) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "x".into(),
                file_path: "f.rs".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: "diff".into(),
            },
            commit: CommitMeta {
                commit_sha: "x".into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts,
                message: "m".into(),
            },
            similarity: 1.0,
        }
    }

    #[tokio::test]
    async fn recency_refiner_newer_hit_ranks_higher() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(2, now - 100 * day), // older
            make_hit(1, now - day),       // newer
        ];
        let weights = RankingWeights::default();
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].hunk_id, 1,
            "newer hit (1 day old) must outrank older (100 days old)"
        );
        assert_eq!(out[1].hunk_id, 2);
    }

    #[tokio::test]
    async fn recency_refiner_zero_weight_preserves_input_order() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![make_hit(10, now - day), make_hit(11, now - 200 * day)];
        let weights = RankingWeights {
            recency_weight: 0.0, // disable tie-break
            ..RankingWeights::default()
        };
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        assert_eq!(out[0].hunk_id, 10);
        assert_eq!(out[1].hunk_id, 11);
    }
}
