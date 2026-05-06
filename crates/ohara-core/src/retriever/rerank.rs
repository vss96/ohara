//! Cross-encoder rerank step.
//!
//! One free async function — [`cross_encode`] — that calls a
//! [`RerankProvider`] (production: BGE-reranker-base) and writes a
//! sigmoid-normalized relevance score into each [`HunkHit::similarity`].
//!
//! Sigmoid-bounding the raw signed logit into `(0, 1)` is load-bearing
//! for downstream multiplicative composition with the recency factor:
//! see plan-22 for the bug it fixed.

use crate::embed::RerankProvider;
use crate::storage::HunkHit;

/// Numerically-stable logistic sigmoid mapping `(-∞, +∞) → (0, 1)`.
fn sigmoid(x: f32) -> f32 {
    if x.is_sign_positive() {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// Score `hits` with `reranker` against `query_text`, sigmoid-normalize
/// the returned logits into `HunkHit::similarity`, and re-sort
/// highest-first. Empty input is returned unchanged without invoking
/// the reranker.
pub async fn cross_encode(
    reranker: &dyn RerankProvider,
    query_text: &str,
    hits: Vec<HunkHit>,
) -> crate::Result<Vec<HunkHit>> {
    if hits.is_empty() {
        return Ok(hits);
    }
    let candidates: Vec<&str> = hits.iter().map(|h| h.hunk.diff_text.as_str()).collect();
    let scores = reranker.rerank(query_text, &candidates).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::RerankProvider;
    use crate::storage::{HunkHit, HunkId};
    use async_trait::async_trait;

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
    async fn cross_encode_reorders_by_score() {
        let hits = vec![
            make_hit(100, "diff-a"),
            make_hit(101, "diff-b"),
            make_hit(102, "diff-c"),
        ];
        let reranker = ScriptedReranker(vec![2.0, 1.0, 3.0]);
        let out = cross_encode(&reranker, "query", hits).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].hunk_id, 102, "highest score (3.0) must be first");
        assert_eq!(out[1].hunk_id, 100, "second score (2.0) must be second");
        assert_eq!(out[2].hunk_id, 101, "lowest score (1.0) must be last");
    }

    #[tokio::test]
    async fn cross_encode_empty_input_returns_empty() {
        let reranker = ScriptedReranker(vec![]);
        let out = cross_encode(&reranker, "q", vec![]).await.unwrap();
        assert!(out.is_empty());
    }
}
