//! Retrieval coordinator.
//!
//! I/O orchestration around the pure ranking pipeline:
//! 1. Fire all lanes in parallel via `join_all`.
//! 2. Hand per-lane rankings to [`ranking::fuse_to_pool`] (RRF + truncate).
//! 3. Optionally cross-encode the pool via [`rerank::cross_encode`].
//! 4. Apply the recency multiplier via [`ranking::apply_recency`].
//! 5. Truncate to caller's `final_k`.
//!
//! Steps 2 and 4 are pure data; step 3 is the only impure step inside
//! the pipeline. Step 1 talks to storage via the lane trait.

use crate::embed::RerankProvider;
use crate::perf_trace::timed_phase;
use crate::query::PatternQuery;
use crate::retriever::{ranking, rerank, RankingWeights, RetrievalLane};
use crate::storage::{HunkHit, HunkId};
use crate::types::RepoId;
use futures::future::join_all;
use std::collections::HashMap;
use std::sync::Arc;

/// Run the full coordinator pipeline.
///
/// - `lanes`: all lane instances (disabled lanes self-skip by returning empty).
/// - `weights`: ranking knobs — RRF k, rerank pool size, recency, etc.
///   The caller is responsible for folding any profile overrides into
///   `weights.recency_weight` before calling.
/// - `reranker`: when `Some`, the cross-encoder runs on the post-RRF pool.
///   When `None`, that step is skipped (degraded mode: post-RRF order
///   with recency multiplier still applied).
/// - `final_k`: hard truncation after recency.
/// - `now_unix`: timestamp the recency multiplier ages against.
pub async fn run(
    lanes: &[Box<dyn RetrievalLane>],
    weights: &RankingWeights,
    reranker: Option<&Arc<dyn RerankProvider>>,
    query: &PatternQuery,
    repo_id: &RepoId,
    final_k: usize,
    now_unix: i64,
) -> crate::Result<Vec<HunkHit>> {
    // 1. Fire all lanes in parallel. Disabled lanes return Ok(vec![])
    //    without touching storage.
    let lane_futures = lanes.iter().map(|l| l.search(query, repo_id, final_k));
    let lane_results: Vec<crate::Result<Vec<HunkHit>>> = join_all(lane_futures).await;

    // 2. Build per-lane ranked id lists + a HunkId -> HunkHit lookup.
    //    The first lane to report an id wins for similarity; downstream
    //    rerank/recency steps overwrite this anyway.
    let mut by_id: HashMap<HunkId, HunkHit> = HashMap::new();
    let mut rankings: Vec<Vec<HunkId>> = Vec::with_capacity(lanes.len());
    for result in lane_results {
        let hits = result?;
        let ranking: Vec<HunkId> = hits
            .iter()
            .map(|h| {
                by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
                h.hunk_id
            })
            .collect();
        rankings.push(ranking);
    }

    // 3. RRF fuse + truncate to rerank pool.
    let pool: Vec<HunkHit> = timed_phase("rrf", async {
        ranking::fuse_to_pool(&rankings, &by_id, weights.rrf_k, weights.rerank_top_k)
    })
    .await;

    if pool.is_empty() {
        return Ok(vec![]);
    }

    // 4. Optional cross-encoder rerank.
    let mut hits = pool;
    if let Some(r) = reranker {
        hits = rerank::cross_encode(r.as_ref(), &query.query, hits).await?;
    }

    // 5. Recency multiplier (writes combined score back into similarity).
    hits = ranking::apply_recency(
        hits,
        weights.recency_weight,
        weights.recency_half_life_days,
        now_unix,
    );

    // 6. Truncate to caller's final_k.
    hits.truncate(final_k);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::retriever::{LaneId, RetrievalLane};
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;

    fn make_hit(id: HunkId, sim: f32) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
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
                ts: 1_700_000_000,
                message: "m".into(),
            },
            similarity: sim,
        }
    }

    struct StaticLane(LaneId, Vec<HunkHit>);

    #[async_trait]
    impl RetrievalLane for StaticLane {
        fn id(&self) -> LaneId {
            self.0
        }
        async fn search(
            &self,
            _: &PatternQuery,
            _: &RepoId,
            _: usize,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.1.clone())
        }
    }

    fn default_query() -> PatternQuery {
        PatternQuery {
            query: "test".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        }
    }

    fn weights_with_zero_recency() -> RankingWeights {
        RankingWeights {
            recency_weight: 0.0,
            ..RankingWeights::default()
        }
    }

    #[tokio::test]
    async fn coordinator_rrf_merges_lanes() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(
                LaneId::Vec,
                vec![make_hit(1, 0.9), make_hit(2, 0.5)],
            )),
            Box::new(StaticLane(
                LaneId::Bm25Text,
                vec![make_hit(2, 0.8), make_hit(1, 0.3)],
            )),
            Box::new(StaticLane(LaneId::Bm25HistSym, vec![make_hit(3, 0.4)])),
        ];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &weights, None, &q, &repo_id, 20, 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(out.len(), 3, "all three unique ids survive rrf");
        assert!(
            out.iter().position(|h| h.hunk_id == 3).unwrap() > 0,
            "id=3 (single-lane) must rank below the two-lane ids"
        );
    }

    #[tokio::test]
    async fn coordinator_threads_rrf_k_into_fusion() {
        // The math is unit-tested in `query::reciprocal_rank_fusion`;
        // this covers the wiring — a non-default k flows through without
        // panic and produces the expected count.
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(LaneId::Vec, vec![make_hit(1, 0.9)])),
            Box::new(StaticLane(LaneId::Bm25Text, vec![make_hit(2, 0.8)])),
        ];
        let weights = RankingWeights {
            rrf_k: 1,
            ..weights_with_zero_recency()
        };
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &weights, None, &q, &repo_id, 20, 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(out.len(), 2, "both unique ids survive rrf with k=1");
    }

    #[tokio::test]
    async fn coordinator_skips_rerank_when_reranker_is_none() {
        // Without a reranker, similarity comes from the recency multiplier
        // alone (when recency_weight=0, similarity stays at the lane's
        // first-reported score).
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(StaticLane(
            LaneId::Vec,
            vec![make_hit(10, 0.9), make_hit(11, 0.5)],
        ))];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &weights, None, &q, &repo_id, 20, 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(out[0].hunk_id, 10);
        assert!((out[0].similarity - 0.9).abs() < 1e-5);
    }
}
