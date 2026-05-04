//! Plan 20 — cross-encoder rerank refiner.

use super::ScoreRefiner;
use crate::embed::RerankProvider;
use crate::storage::HunkHit;
use async_trait::async_trait;
use std::sync::Arc;

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
        // Zip hits with scores, write reranker score into similarity so
        // downstream refiners (e.g. RecencyRefiner) use it as the base.
        let mut scored: Vec<(HunkHit, f32)> = hits
            .into_iter()
            .zip(scores)
            .map(|(mut h, s)| {
                h.similarity = s;
                (h, s)
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
