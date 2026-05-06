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
/// **Score domain:** unbounded `f32`. Implementations MAY return raw
/// cross-encoder logits (the `fastembed::TextRerank` impl in
/// `ohara-embed` does), which means scores CAN be negative for
/// low-relevance pairs. Downstream consumers in `crate::retriever`
/// sigmoid-normalise the score before any multiplicative combination
/// (see plan-22). New implementations MUST NOT silently apply their
/// own normalisation that would clamp the score into `[0, 1]`, since
/// that would lose the relative-ordering signal cross-encoders
/// produce in the negative range.
///
/// Implementations MUST be order-preserving with respect to the input
/// slice (i.e. the returned `Vec<f32>` aligns positionally with
/// `candidates`).
#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
}

/// Plan-27 chunk-embed cache mode. Selects whether the embedder is
/// fronted by the `chunk_embed_cache` table and, in `Diff` mode,
/// what input the embedder consumes.
///
/// - `Off`: no cache; embedder consumes today's `effective_semantic_text`.
/// - `Semantic`: cache keyed by `sha256(effective_semantic_text)`;
///   embedder input unchanged.
/// - `Diff`: cache keyed by `sha256(diff_text)`; embedder input is
///   `diff_text` only (commit message dropped from the vector lane).
///
/// `Off` and `Semantic` produce vector-equivalent indices (same
/// embedder input). `Diff` produces a different vector lane and so
/// requires a `--rebuild` to switch into or out of.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum EmbedMode {
    #[default]
    Off,
    Semantic,
    Diff,
}

impl EmbedMode {
    /// Stable string used as the `embed_input_mode` value in
    /// `RuntimeIndexMetadata`. Off and Semantic share `"semantic"`
    /// because they're vector-equivalent; Diff has its own class.
    pub fn index_metadata_value(self) -> &'static str {
        match self {
            EmbedMode::Off | EmbedMode::Semantic => "semantic",
            EmbedMode::Diff => "diff",
        }
    }
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
            Ok(candidates.iter().map(|s| s.len() as f32).collect())
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

#[cfg(test)]
mod embed_mode_tests {
    use super::*;

    #[test]
    fn embed_mode_off_and_semantic_are_distinct_variants() {
        assert_ne!(EmbedMode::Off, EmbedMode::Semantic);
        assert_ne!(EmbedMode::Off, EmbedMode::Diff);
        assert_ne!(EmbedMode::Semantic, EmbedMode::Diff);
    }

    #[test]
    fn embed_mode_default_is_off() {
        // Plan 27 Task B.2: the default mode must match today's
        // behavior — no cache lookups.
        assert_eq!(EmbedMode::default(), EmbedMode::Off);
    }

    #[test]
    fn embed_mode_index_metadata_value_distinguishes_diff() {
        // Plan 27 Task B.2: Off and Semantic both embed semantic_text
        // and so are vector-equivalent; they share the same
        // index_metadata value. Diff is a separate compatibility class.
        assert_eq!(EmbedMode::Off.index_metadata_value(), "semantic");
        assert_eq!(EmbedMode::Semantic.index_metadata_value(), "semantic");
        assert_eq!(EmbedMode::Diff.index_metadata_value(), "diff");
    }
}
