//! Tunable ranking weights for the retrieval pipeline.

/// Tunable knobs for the retrieval pipeline.
#[derive(Debug, Clone)]
pub struct RankingWeights {
    /// Multiplier on the recency factor in the final score:
    /// `final = sigmoid(rerank) * (1.0 + recency_weight * exp(-age_days / half_life_days))`.
    ///
    /// `sigmoid(rerank)` bounds the cross-encoder's signed logit into
    /// `(0, 1)` so the multiplicative recency factor always boosts in
    /// the expected direction (more recent ⇒ higher combined score).
    /// See plan-22 for the bug this fixed. The sigmoid is applied
    /// inside `refiners::cross_encoder::CrossEncoderRefiner` before
    /// the score lands in `HunkHit::similarity`, so the
    /// `RecencyRefiner` sees an already-bounded base.
    ///
    /// Default 0.05 — small enough to act as a tie-breaker without
    /// overpowering rerank quality.
    pub recency_weight: f32,
    /// Half-life-ish constant (in days) for the exp-decay recency factor.
    /// Default 90.0 — a 90-day-old commit gets factor ≈ 0.37.
    pub recency_half_life_days: f32,
    /// Number of post-RRF candidates fed into the cross-encoder.
    /// Default 20 — plan-23's sweep against the context-engine eval
    /// fixture showed the smallest pool already on the recall plateau,
    /// so the larger pools paid extra cross-encoder cost for no recall
    /// gain. See `tests/perf/baselines/rerank_pool_sweep.jsonl`.
    pub rerank_top_k: usize,
    /// Per-lane gather size before RRF. Default 100. Must fit in `u8` because
    /// the storage trait uses `u8` for `k` arguments.
    pub lane_top_k: u8,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            recency_weight: 0.05,
            recency_half_life_days: 90.0,
            rerank_top_k: 20,
            lane_top_k: 100,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RankingWeights;

    #[test]
    fn ranking_weights_default_rerank_pool_matches_plan_23_baseline() {
        // plan-23 sweep concluded the recommended pool is 20 (smallest
        // pool whose recall_at_5 is within 1% of the best observed and
        // whose p95_ms is within 1.5x of the smallest observed). Pin
        // the value so a future drift triggers a CI failure and forces
        // re-running the sweep. See:
        //   tests/perf/baselines/rerank_pool_sweep.jsonl
        //   docs/superpowers/plans/2026-05-05-ohara-plan-23-rerank-pool-sizing.md
        assert_eq!(RankingWeights::default().rerank_top_k, 20);
    }
}
