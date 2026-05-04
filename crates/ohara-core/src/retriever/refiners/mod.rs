pub mod cross_encoder;
pub mod recency;

#[cfg(test)]
mod trait_object_tests {
    use super::*;
    use crate::storage::HunkHit;
    use async_trait::async_trait;

    struct PassthroughRefiner;

    #[async_trait]
    impl ScoreRefiner for PassthroughRefiner {
        async fn refine(
            &self,
            _query_text: &str,
            hits: Vec<HunkHit>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn score_refiner_is_object_safe() {
        let refiner: Box<dyn ScoreRefiner> = Box::new(PassthroughRefiner);
        let hits: Vec<HunkHit> = vec![];
        let out = refiner.refine("q", hits).await.unwrap();
        assert!(out.is_empty());
    }
}
