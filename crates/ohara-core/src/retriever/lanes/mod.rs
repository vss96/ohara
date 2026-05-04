pub mod bm25_head_sym;
pub mod bm25_hist_sym;
pub mod bm25_text;
pub mod vec;

#[cfg(test)]
mod trait_object_tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::storage::HunkHit;
    use crate::types::RepoId;
    use async_trait::async_trait;

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
