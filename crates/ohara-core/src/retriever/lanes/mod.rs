//! Plan 20 — retrieval lane abstractions.
//!
//! Each lane encapsulates one candidate-gathering strategy (vector KNN,
//! BM25-by-text, BM25-by-historical-symbol, BM25-by-head-symbol). The
//! coordinator fires all enabled lanes via `join_all` and merges their
//! results with Reciprocal Rank Fusion.
//!
//! Lane implementations live in sibling modules:
//!   vec, bm25_text, bm25_hist_sym, bm25_head_sym.

use crate::query::PatternQuery;
use crate::storage::HunkHit;
use crate::types::RepoId;
use async_trait::async_trait;

pub mod bm25_head_sym;
pub mod bm25_hist_sym;
pub mod bm25_text;
pub mod vec;

/// Stable identifier for each retrieval lane. Used by
/// `RetrievalProfile::is_lane_enabled` so the coordinator can ask each lane
/// whether its profile flag is set without knowing the concrete type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneId {
    Vec,
    Bm25Text,
    Bm25HistSym,
    Bm25HeadSym,
}

/// One retrieval strategy.
///
/// Implementors query their respective storage method and return an ordered
/// `Vec<HunkHit>` from most to least relevant according to that lane's
/// scoring function. The caller (coordinator) merges lanes via RRF —
/// lane-internal scores are used only for the informational
/// `HunkHit::similarity` field.
///
/// Each implementation checks
/// `query.profile.is_lane_enabled(self.id())` as its first step and returns
/// `Ok(vec![])` when the lane is disabled by the profile (option a — lanes
/// self-gate). This keeps the coordinator dumb: it always fires all lanes
/// via `join_all` and trusts disabled lanes to return empty without touching
/// storage.
#[async_trait]
pub trait RetrievalLane: Send + Sync {
    fn id(&self) -> LaneId;
    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>>;
}

#[cfg(test)]
mod trait_object_tests {
    use super::*;

    struct DummyLane(LaneId);

    #[async_trait]
    impl RetrievalLane for DummyLane {
        fn id(&self) -> LaneId {
            self.0
        }
        async fn search(
            &self,
            _query: &PatternQuery,
            _repo_id: &RepoId,
            _k: usize,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn retrieval_lane_is_object_safe() {
        let lane: Box<dyn RetrievalLane> = Box::new(DummyLane(LaneId::Vec));
        assert_eq!(lane.id(), LaneId::Vec);
        let q = PatternQuery {
            query: "test".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &id, 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn lane_id_variants_are_distinct() {
        assert_ne!(LaneId::Vec, LaneId::Bm25Text);
        assert_ne!(LaneId::Bm25Text, LaneId::Bm25HistSym);
        assert_ne!(LaneId::Bm25HistSym, LaneId::Bm25HeadSym);
    }
}
