//! Plan 20 — cross-encoder rerank refiner.

use super::ScoreRefiner;
use crate::embed::RerankProvider;
use crate::storage::HunkHit;
use async_trait::async_trait;
use std::sync::Arc;

/// Numerically-stable logistic sigmoid, mapping `(-∞, +∞) → (0, 1)`.
///
/// Plan 22: `bge-reranker-base` returns raw signed logits (negative for
/// low-relevance pairs). Downstream refiners — notably
/// [`crate::retriever::refiners::recency::RecencyRefiner`] —
/// multiply by a positive recency factor, and
/// `negative * (1 + small) > negative * (1 + large)` flips the
/// expected "more recent ⇒ higher combined score" ordering. Sigmoid-
/// bounding the rerank score into `(0, 1)` before it lands in
/// `HunkHit::similarity` removes the sign ambiguity for every
/// downstream multiplicative composition. The branch on
/// `is_sign_positive()` avoids `exp` overflow for large-magnitude
/// inputs in either direction.
fn sigmoid(x: f32) -> f32 {
    if x.is_sign_positive() {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// Reranks candidates with an injected `RerankProvider` (BGE-reranker-base
/// in production). The refiner does not own a semaphore — the caller
/// (coordinator or daemon) holds one around the full pipeline step if
/// needed, as it does today.
pub struct CrossEncoderRefiner {
    reranker: Arc<dyn RerankProvider>,
}

impl CrossEncoderRefiner {
    pub fn new(reranker: Arc<dyn RerankProvider>) -> Self {
        Self { reranker }
    }
}

#[async_trait]
impl ScoreRefiner for CrossEncoderRefiner {
    async fn refine(&self, query_text: &str, hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        let candidates: Vec<&str> = hits.iter().map(|h| h.hunk.diff_text.as_str()).collect();
        let scores = self.reranker.rerank(query_text, &candidates).await?;
        // Plan 22: sigmoid-normalise the raw cross-encoder logit before
        // writing it into `similarity`. Downstream `RecencyRefiner`
        // does `similarity * (1 + α * recency)`; without the sigmoid,
        // two equally-bad candidates with negative logits would order
        // older-above-newer because `negative * (1 + small)` is less
        // negative than `negative * (1 + large)`. Sorting still uses
        // the normalised score so ordering is monotonic with the raw
        // logit (sigmoid is strictly increasing).
        let mut scored: Vec<(HunkHit, f32)> = hits
            .into_iter()
            .zip(scores)
            .map(|(mut h, s)| {
                let s_norm = sigmoid(s);
                h.similarity = s_norm;
                (h, s_norm)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().map(|(h, _)| h).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::RerankProvider;
    use crate::storage::{HunkHit, HunkId};
    use async_trait::async_trait;
    use std::sync::Arc;

    fn make_hit(id: HunkId, diff: &str) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "x".into(),
                file_path: "f.rs".into(),
                language: None,
                change_kind: ChangeKind::Added,
                diff_text: diff.into(),
            },
            commit: CommitMeta {
                commit_sha: "x".into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 0,
                message: "m".into(),
            },
            similarity: 0.5,
        }
    }

    struct ScriptedReranker(Vec<f32>);

    #[async_trait]
    impl RerankProvider for ScriptedReranker {
        async fn rerank(&self, _: &str, candidates: &[&str]) -> crate::Result<Vec<f32>> {
            assert_eq!(candidates.len(), self.0.len(), "score count mismatch");
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn cross_encoder_refiner_reorders_by_score() {
        let hits = vec![
            make_hit(100, "diff-a"),
            make_hit(101, "diff-b"),
            make_hit(102, "diff-c"),
        ];
        let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker(vec![2.0, 1.0, 3.0]));
        let refiner = CrossEncoderRefiner::new(reranker);
        let out = refiner.refine("query", hits).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].hunk_id, 102, "highest score (3.0) must be first");
        assert_eq!(out[1].hunk_id, 100, "second score (2.0) must be second");
        assert_eq!(out[2].hunk_id, 101, "lowest score (1.0) must be last");
    }

    #[tokio::test]
    async fn cross_encoder_refiner_empty_input_returns_empty() {
        let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker(vec![]));
        let refiner = CrossEncoderRefiner::new(reranker);
        let out = refiner.refine("q", vec![]).await.unwrap();
        assert!(out.is_empty());
    }
}
