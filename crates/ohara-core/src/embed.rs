use crate::Result;
use async_trait::async_trait;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dimension(&self) -> usize;

    fn model_id(&self) -> &str;

    /// Embed a batch of texts. The output has the same length and order as the input.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Cross-encoder reranker contract.
///
/// Score `candidates` against `query`. Output length == `candidates.len()`;
/// element `i` is the score for `candidates[i]`. Higher is better.
///
/// Implementations MUST be order-preserving with respect to the input slice
/// (i.e. the returned `Vec<f32>` aligns positionally with `candidates`).
#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-tree fake reranker that scores each candidate by its character
    /// length. Documents the order-preserving contract: the score at index
    /// `i` corresponds to `candidates[i]`, regardless of the underlying
    /// model's preferred ordering.
    struct FakeReranker;

    #[async_trait]
    impl RerankProvider for FakeReranker {
        async fn rerank(&self, _query: &str, candidates: &[&str]) -> Result<Vec<f32>> {
            // Deliberately broken in B.1.r: returns an empty vec rather than
            // one-score-per-candidate. B.1.g fixes this to match the contract.
            let _ = candidates;
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn rerank_provider_is_order_preserving() {
        let r = FakeReranker;
        let candidates = ["alpha", "beta-beta", "g"];
        let out = r.rerank("query", &candidates).await.unwrap();
        assert_eq!(out.len(), candidates.len(), "score-per-candidate contract");
        // FakeReranker returns char-length so we can verify positional alignment.
        assert_eq!(out, vec![5.0, 9.0, 1.0]);
    }
}
