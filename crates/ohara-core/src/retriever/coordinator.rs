//! Plan 20 — retrieval coordinator.
//!
//! Wires the 5-step pipeline:
//! 1. Fire all lanes in parallel via `join_all`.
//! 2. RRF-merge into one ranked list (free function — not a trait).
//! 3. Truncate to `rerank_pool_k` before the expensive refiners.
//! 4. Apply each `ScoreRefiner` in sequence.
//! 5. Truncate to caller's `k`.

use crate::perf_trace::timed_phase;
use crate::query::{reciprocal_rank_fusion, PatternQuery};
use crate::retriever::{RetrievalLane, ScoreRefiner};
use crate::storage::{HunkHit, HunkId};
use crate::types::RepoId;
use futures::future::join_all;
use std::collections::HashMap;

/// Run the full coordinator pipeline.
///
/// - `lanes`: all lane instances (disabled lanes self-skip by returning empty).
/// - `refiners`: applied in order to the post-RRF candidate list.
/// - `rerank_pool_k`: how many post-RRF candidates to feed into refiners.
/// - `final_k`: hard truncation after refiners.
pub async fn run(
    lanes: &[Box<dyn RetrievalLane>],
    refiners: &[Box<dyn ScoreRefiner>],
    query: &PatternQuery,
    repo_id: &RepoId,
    rerank_pool_k: usize,
    final_k: usize,
) -> crate::Result<Vec<HunkHit>> {
    // 1. Fire all lanes in parallel. Disabled lanes return Ok(vec![])
    //    without touching storage.
    let lane_futures = lanes.iter().map(|l| l.search(query, repo_id, final_k));
    let lane_results: Vec<crate::Result<Vec<HunkHit>>> = join_all(lane_futures).await;

    // 2. Build per-lane ranked id lists + a HunkId -> HunkHit lookup.
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

    // 3. RRF merge (k=60, Cormack 2009) → truncate to rerank pool.
    let fused: Vec<HunkId> = timed_phase("rrf", async {
        reciprocal_rank_fusion(&rankings, 60)
    })
    .await;
    let pool: Vec<HunkHit> = fused
        .into_iter()
        .take(rerank_pool_k)
        .filter_map(|id| by_id.get(&id).cloned())
        .collect();

    if pool.is_empty() {
        return Ok(vec![]);
    }

    // 4. Apply refiners in sequence.
    let mut hits = pool;
    for refiner in refiners {
        hits = refiner.refine(&query.query, hits).await?;
    }

    // 5. Truncate to final k.
    hits.truncate(final_k);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::retriever::{LaneId, RetrievalLane, ScoreRefiner};
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;

    fn make_hit(id: HunkId, sim: f32) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "x".into(), file_path: "f.rs".into(), language: None, change_kind: ChangeKind::Added, diff_text: format!("diff-{id}") },
            commit: CommitMeta { commit_sha: "x".into(), parent_sha: None, is_merge: false, author: None, ts: 1_700_000_000, message: "m".into() },
            similarity: sim,
        }
    }

    struct StaticLane(LaneId, Vec<HunkHit>);

    #[async_trait]
    impl RetrievalLane for StaticLane {
        fn id(&self) -> LaneId { self.0 }
        async fn search(&self, _: &PatternQuery, _: &RepoId, _: usize) -> crate::Result<Vec<HunkHit>> {
            Ok(self.1.clone())
        }
    }

    struct IdentityRefiner;

    #[async_trait]
    impl ScoreRefiner for IdentityRefiner {
        async fn refine(&self, _: &str, hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn coordinator_rrf_merges_lanes() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(LaneId::Vec, vec![make_hit(1, 0.9), make_hit(2, 0.5)])),
            Box::new(StaticLane(LaneId::Bm25Text, vec![make_hit(2, 0.8), make_hit(1, 0.3)])),
            Box::new(StaticLane(LaneId::Bm25HistSym, vec![make_hit(3, 0.4)])),
        ];
        let refiners: Vec<Box<dyn ScoreRefiner>> = vec![Box::new(IdentityRefiner)];
        let q = PatternQuery { query: "test".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &refiners, &q, &repo_id, 10, 20).await.unwrap();
        assert_eq!(out.len(), 3, "all three unique ids survive rrf");
        assert!(out.iter().position(|h| h.hunk_id == 3).unwrap() > 0,
            "id=3 (single-lane) must rank below the two-lane ids");
    }

    #[tokio::test]
    async fn coordinator_applies_refiners_in_sequence() {
        struct ReverseRefiner;
        #[async_trait]
        impl ScoreRefiner for ReverseRefiner {
            async fn refine(&self, _: &str, mut hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
                hits.reverse();
                Ok(hits)
            }
        }
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(LaneId::Vec, vec![make_hit(10, 0.9), make_hit(11, 0.5)])),
        ];
        let refiners: Vec<Box<dyn ScoreRefiner>> = vec![Box::new(ReverseRefiner)];
        let q = PatternQuery { query: "anything".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &refiners, &q, &repo_id, 10, 20).await.unwrap();
        assert_eq!(out[0].hunk_id, 11);
        assert_eq!(out[1].hunk_id, 10);
    }
}
