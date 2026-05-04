//! Plan 20 — post-RRF score refiners.
//!
//! A `ScoreRefiner` takes the RRF-merged `Vec<HunkHit>` and returns a
//! reordered/rescored version. The coordinator applies a sequence of
//! refiners in order:
//!
//! ```text
//! for refiner in refiners {
//!     hits = refiner.refine(query_text, hits).await?;
//! }
//! ```
//!
//! Implementations live in sibling modules:
//!   cross_encoder, recency.

use crate::storage::HunkHit;
use async_trait::async_trait;

pub mod cross_encoder;
pub mod recency;

/// One post-RRF transformation step.
///
/// Refiners receive the full ordered candidate list and return a new
/// ordered list. They may reorder, rescore, or prune candidates.
/// Returning the list in the same order is a valid (no-op) implementation.
///
/// The `query_text` parameter is the raw query string. Cross-encoder
/// refiners use it for relevance scoring; recency refiners ignore it.
#[async_trait]
pub trait ScoreRefiner: Send + Sync {
    async fn refine(
        &self,
        query_text: &str,
        hits: Vec<HunkHit>,
    ) -> crate::Result<Vec<HunkHit>>;
}

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
