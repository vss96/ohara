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
use crate::visibility::VisibilityScope;
use crate::CommitSha;
use futures::future::join_all;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Per-call inputs to [`run`].
pub struct RunContext<'a> {
    /// The query being served.
    pub query: &'a PatternQuery,
    /// Repo the lanes search.
    pub repo_id: &'a RepoId,
    /// Hard truncation after recency.
    pub final_k: usize,
    /// Timestamp the recency multiplier ages against.
    pub now_unix: i64,
    /// When `Some`, candidates not reachable from the scope's HEAD are
    /// dropped before RRF fusion. `None` leaves behaviour unchanged.
    pub visibility: Option<VisibilityScope<'a>>,
}

/// Run the full coordinator pipeline.
///
/// - `lanes`: all lane instances (disabled lanes self-skip by returning empty).
/// - `weights`: ranking knobs — RRF k, rerank pool size, recency, etc.
///   The caller is responsible for folding any profile overrides into
///   `weights.recency_weight` before calling.
/// - `reranker`: when `Some`, the cross-encoder runs on the post-RRF pool.
///   When `None`, that step is skipped (degraded mode: post-RRF order
///   with recency multiplier still applied).
/// - `context`: per-call inputs (see [`RunContext`]).
pub async fn run(
    lanes: &[Box<dyn RetrievalLane>],
    weights: &RankingWeights,
    reranker: Option<&Arc<dyn RerankProvider>>,
    context: RunContext<'_>,
) -> crate::Result<Vec<HunkHit>> {
    let RunContext {
        query,
        repo_id,
        final_k,
        now_unix,
        visibility,
    } = context;
    // 1. Fire all lanes in parallel. Disabled lanes return Ok(vec![])
    //    without touching storage. Lanes gather up to `lane_top_k`
    //    candidates each (the documented per-lane fan-in for RRF);
    //    the caller's `final_k` truncation is applied at step 6 only.
    let lane_k = weights.lane_top_k as usize;
    let lane_futures = lanes.iter().map(|l| l.search(query, repo_id, lane_k));
    let lane_results: Vec<crate::Result<Vec<HunkHit>>> = join_all(lane_futures).await;

    // 2. Build per-lane ranked id lists + a HunkId -> Arc<HunkHit> lookup.
    //    The first lane to report an id wins for similarity; downstream
    //    rerank/recency steps overwrite this anyway. Each hit is moved
    //    into the map once (no clone) and shared by refcount thereafter.
    let mut by_id: HashMap<HunkId, Arc<HunkHit>> = HashMap::new();
    let mut rankings: Vec<Vec<HunkId>> = Vec::with_capacity(lanes.len());
    for result in lane_results {
        let hits = result?;
        let ranking: Vec<HunkId> = hits
            .into_iter()
            .map(|h| {
                let id = h.hunk_id;
                // `or_insert_with` consumes `h` only on the first sighting
                // of an id; later duplicates from other lanes drop it.
                by_id.entry(id).or_insert_with(|| Arc::new(h));
                id
            })
            .collect();
        rankings.push(ranking);
    }

    // 2b. Drop candidates the querying worktree cannot see, before the
    //     rerank pool is cut so invisible hits never crowd out visible ones.
    if let Some(scope) = visibility {
        retain_visible(&mut rankings, &mut by_id, scope).await?;
    }

    // 3. RRF fuse + truncate to rerank pool. `fuse_to_pool` shares hits
    //    via `Arc::clone` (refcount bumps), not deep clones.
    let arc_pool: Vec<Arc<HunkHit>> = timed_phase("rrf", async {
        ranking::fuse_to_pool(&rankings, &by_id, weights.rrf_k, weights.rerank_top_k)
    })
    .await;

    if arc_pool.is_empty() {
        return Ok(vec![]);
    }

    // Materialize owned hits for the rerank/recency boundary, which
    // mutates `similarity`. RRF dedups ids, so each pooled `Arc` is
    // unique; dropping `by_id` releases its refs so `try_unwrap`
    // reclaims every survivor in place — no deep clone. (The fallback
    // clone only fires if an `Arc` is unexpectedly still shared, which
    // the legacy code did unconditionally for every hit, twice.)
    drop(by_id);
    let pool: Vec<HunkHit> = arc_pool
        .into_iter()
        .map(|h| Arc::try_unwrap(h).unwrap_or_else(|h| (*h).clone()))
        .collect();

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

/// Ask the oracle once about every distinct commit in `by_id`, then remove
/// invisible hunks from `by_id` and every ranking. A sha that is not a valid
/// `CommitSha` is invisible.
async fn retain_visible(
    rankings: &mut [Vec<HunkId>],
    by_id: &mut HashMap<HunkId, Arc<HunkHit>>,
    scope: VisibilityScope<'_>,
) -> crate::Result<()> {
    let mut distinct: Vec<CommitSha> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for hit in by_id.values() {
        let raw = hit.commit.commit_sha.as_str();
        if !seen.insert(raw) {
            continue;
        }
        if let Ok(parsed) = CommitSha::parse(raw) {
            distinct.push(parsed);
        }
    }
    if distinct.is_empty() {
        by_id.clear();
        rankings.iter_mut().for_each(Vec::clear);
        return Ok(());
    }
    let verdicts = scope.oracle.visible(scope.head, &distinct).await?;
    if verdicts.len() != distinct.len() {
        return Err(crate::OhraError::Git(format!(
            "visibility oracle returned {} verdicts for {} commits",
            verdicts.len(),
            distinct.len()
        )));
    }
    let visible: HashSet<&str> = distinct
        .iter()
        .zip(verdicts)
        .filter(|(_, ok)| *ok)
        .map(|(c, _)| c.as_str())
        .collect();
    by_id.retain(|_, h| visible.contains(h.commit.commit_sha.as_str()));
    for ranking in rankings.iter_mut() {
        ranking.retain(|id| by_id.contains_key(id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::retriever::{LaneId, RetrievalLane};
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use crate::visibility::{CommitVisibility, VisibilityScope};
    use crate::CommitSha;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::Mutex;

    fn ctx<'a>(
        query: &'a PatternQuery,
        repo_id: &'a RepoId,
        final_k: usize,
        visibility: Option<VisibilityScope<'a>>,
    ) -> RunContext<'a> {
        RunContext {
            query,
            repo_id,
            final_k,
            now_unix: 1_700_000_000,
            visibility,
        }
    }

    /// A real 40-hex sha derived from `n`.
    fn sha(n: u32) -> String {
        format!("{n:040x}")
    }

    fn make_hit_on(id: HunkId, commit_sha: String) -> HunkHit {
        let mut h = make_hit(id, 0.9 - (id as f32) * 0.01);
        h.hunk.commit_sha = commit_sha.clone();
        h.commit.commit_sha = commit_sha;
        h
    }

    /// Visible iff the sha is in the set; records every call's `commits`.
    struct SetOracle {
        visible: HashSet<String>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl SetOracle {
        fn new(visible: &[u32]) -> Self {
            Self {
                visible: visible.iter().map(|n| sha(*n)).collect(),
                calls: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl CommitVisibility for SetOracle {
        async fn visible(
            &self,
            _head: &CommitSha,
            commits: &[CommitSha],
        ) -> crate::Result<Vec<bool>> {
            self.calls
                .lock()
                .unwrap()
                .push(commits.iter().map(|c| c.as_str().to_string()).collect());
            Ok(commits
                .iter()
                .map(|c| self.visible.contains(c.as_str()))
                .collect())
        }
    }

    fn head() -> CommitSha {
        CommitSha::parse(&sha(0xfff)).unwrap()
    }

    #[tokio::test]
    async fn invisible_commits_are_removed_before_the_rerank_pool_is_cut() {
        // Best-first on commits [x1, x2, v1, v2]; only v1/v2 visible.
        // Filtering after the pool cut (size 2) would return nothing.
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(StaticLane(
            LaneId::Vec,
            vec![
                make_hit_on(1, sha(1)),
                make_hit_on(2, sha(2)),
                make_hit_on(3, sha(3)),
                make_hit_on(4, sha(4)),
            ],
        ))];
        let weights = RankingWeights {
            rerank_top_k: 2,
            ..weights_with_zero_recency()
        };
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let oracle = SetOracle::new(&[3, 4]);
        let h = head();
        let scope = VisibilityScope {
            head: &h,
            oracle: &oracle,
        };
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, Some(scope)))
            .await
            .unwrap();
        let commits: Vec<String> = out.iter().map(|h| h.commit.commit_sha.clone()).collect();
        assert_eq!(commits, vec![sha(3), sha(4)]);
    }

    /// Returns at most `k` of its hits, like a storage lane.
    struct TopKLane(Vec<HunkHit>);

    #[async_trait]
    impl RetrievalLane for TopKLane {
        fn id(&self) -> LaneId {
            LaneId::Vec
        }
        async fn search(
            &self,
            _: &PatternQuery,
            _: &RepoId,
            k: usize,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.iter().take(k).cloned().collect())
        }
    }

    #[tokio::test]
    async fn invisible_hits_do_not_take_a_lanes_top_k_slots() {
        // The lane's best two hits are invisible; with lane_top_k = 2 a
        // lane cut before filtering would leave no visible candidate.
        let hits: Vec<HunkHit> = (1..=6).map(|n| make_hit_on(n, sha(n as u32))).collect();
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(TopKLane(hits))];
        let weights = RankingWeights {
            lane_top_k: 2,
            ..weights_with_zero_recency()
        };
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let oracle = SetOracle::new(&[3, 4, 5, 6]);
        let h = head();
        let scope = VisibilityScope {
            head: &h,
            oracle: &oracle,
        };
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, Some(scope)))
            .await
            .unwrap();
        let commits: Vec<String> = out.iter().map(|h| h.commit.commit_sha.clone()).collect();
        assert_eq!(
            commits,
            vec![sha(3), sha(4)],
            "the filtered ranking is cut back to lane_top_k visible hits"
        );
    }

    #[tokio::test]
    async fn oracle_is_asked_once_with_distinct_commits() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(
                LaneId::Vec,
                vec![make_hit_on(1, sha(1)), make_hit_on(2, sha(2))],
            )),
            Box::new(StaticLane(
                LaneId::Bm25Text,
                vec![make_hit_on(2, sha(2)), make_hit_on(3, sha(3))],
            )),
        ];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let oracle = SetOracle::new(&[1, 2, 3]);
        let h = head();
        let scope = VisibilityScope {
            head: &h,
            oracle: &oracle,
        };
        run(&lanes, &weights, None, ctx(&q, &repo_id, 20, Some(scope)))
            .await
            .unwrap();
        let calls = oracle.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one oracle call");
        let mut asked = calls[0].clone();
        asked.sort();
        assert_eq!(asked, vec![sha(1), sha(2), sha(3)]);
    }

    #[tokio::test]
    async fn no_scope_means_no_filtering_and_no_oracle_call() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(StaticLane(
            LaneId::Vec,
            vec![make_hit_on(1, sha(1)), make_hit_on(2, "not-hex".into())],
        ))];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, None))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn all_candidates_invisible_returns_empty() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(StaticLane(
            LaneId::Vec,
            vec![make_hit_on(1, sha(1)), make_hit_on(2, sha(2))],
        ))];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let oracle = SetOracle::new(&[]);
        let h = head();
        let scope = VisibilityScope {
            head: &h,
            oracle: &oracle,
        };
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, Some(scope)))
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn non_hex_sha_is_invisible() {
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![Box::new(StaticLane(
            LaneId::Vec,
            vec![make_hit_on(1, "x".into()), make_hit_on(2, sha(2))],
        ))];
        let weights = weights_with_zero_recency();
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        let oracle = SetOracle::new(&[2]);
        let h = head();
        let scope = VisibilityScope {
            head: &h,
            oracle: &oracle,
        };
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, Some(scope)))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hunk_id, 2);
    }

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
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, None))
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
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, None))
            .await
            .unwrap();
        assert_eq!(out.len(), 2, "both unique ids survive rrf with k=1");
    }

    /// Lane that records every `k` it was called with so a regression
    /// test can assert what the coordinator threaded into `.search(...)`.
    struct CapturingLane {
        id: LaneId,
        hits: Vec<HunkHit>,
        observed_k: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl RetrievalLane for CapturingLane {
        fn id(&self) -> LaneId {
            self.id
        }
        async fn search(
            &self,
            _: &PatternQuery,
            _: &RepoId,
            k: usize,
        ) -> crate::Result<Vec<HunkHit>> {
            self.observed_k.lock().unwrap().push(k);
            Ok(self.hits.clone())
        }
    }

    #[tokio::test]
    async fn coordinator_passes_lane_top_k_not_final_k_to_lanes() {
        // Issue #51 regression: `RankingWeights.lane_top_k` (default
        // 100) is the per-lane gather size before RRF — NOT the
        // caller's `final_k` truncation. Pre-fix the coordinator
        // routed `final_k` into `.search(...)`, so each lane gathered
        // only `final_k` candidates and RRF saw a smaller pool than
        // the documented design.
        let observed_vec: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![]));
        let observed_text: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![]));
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(CapturingLane {
                id: LaneId::Vec,
                hits: vec![make_hit(1, 0.9)],
                observed_k: observed_vec.clone(),
            }),
            Box::new(CapturingLane {
                id: LaneId::Bm25Text,
                hits: vec![make_hit(2, 0.8)],
                observed_k: observed_text.clone(),
            }),
        ];
        let weights = RankingWeights {
            lane_top_k: 100,
            ..weights_with_zero_recency()
        };
        let q = default_query();
        let repo_id = RepoId::from_parts("sha", "/repo");
        // Caller asks for final_k = 5; lanes MUST still gather
        // `lane_top_k` = 100 candidates each.
        let _ = run(&lanes, &weights, None, ctx(&q, &repo_id, 5, None))
            .await
            .unwrap();

        let vec_calls = observed_vec.lock().unwrap().clone();
        let text_calls = observed_text.lock().unwrap().clone();
        assert_eq!(vec_calls, vec![100], "vec lane k must equal lane_top_k");
        assert_eq!(text_calls, vec![100], "text lane k must equal lane_top_k");
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
        let out = run(&lanes, &weights, None, ctx(&q, &repo_id, 20, None))
            .await
            .unwrap();
        assert_eq!(out[0].hunk_id, 10);
        assert!((out[0].similarity - 0.9).abs() < 1e-5);
    }
}
