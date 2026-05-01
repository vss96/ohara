//! BGE-small-en-v1.5 (384d) embedding provider over fastembed-rs,
//! plus BGE-reranker-base cross-encoder over `fastembed::TextRerank`.
//!
//! Concurrency: both `embed_batch` and `rerank` offload the ONNX
//! forward pass to `tokio::task::spawn_blocking` and serialize access
//! to the model via `tokio::sync::Mutex` (see field comments for
//! rationale).

use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank,
};
use ohara_core::embed::RerankProvider;
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5";
const DEFAULT_DIM: usize = 384;
const DEFAULT_RERANKER_ID: &str = "bge-reranker-base";

pub struct FastEmbedProvider {
    // Mutex serializes access: fastembed 4.9 holds mutable tokenizer/batch
    // state inside `embed(&self, ...)` and concurrent calls are not audited.
    model: Arc<Mutex<TextEmbedding>>,
    model_id: String,
    dim: usize,
}

impl FastEmbedProvider {
    pub fn new() -> Result<Self> {
        // `InitOptions` is `#[non_exhaustive]` in fastembed v4.9, so it cannot be
        // constructed via struct-literal syntax from outside the crate. Use the
        // builder API (`InitOptions::new(...).with_show_download_progress(...)`)
        // which preserves the plan's intent: load BGE small with downloads silent.
        let opts =
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false);
        let model = TextEmbedding::try_new(opts)
            .context("loading BGE-small model (downloads ~80MB on first run)")?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            model_id: DEFAULT_MODEL_ID.into(),
            dim: DEFAULT_DIM,
        })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = self.model.clone();
        let owned: Vec<String> = texts.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let guard = model.blocking_lock();
            let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            guard.embed(refs, None)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?;
        result.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
    }
}

/// Cross-encoder reranker backed by `fastembed::TextRerank`
/// (BGE-reranker-base, ~110MB on first run).
///
/// fastembed's `rerank` returns `Vec<RerankResult>` sorted by score
/// descending, but our `RerankProvider` contract requires the output
/// `Vec<f32>` to align positionally with the input `candidates` slice.
/// We restore the input ordering before returning (see `align_by_index`).
pub struct FastEmbedReranker {
    // Mutex serializes access for the same reason as FastEmbedProvider:
    // fastembed's rerank() takes &self but uses session state that is
    // not audited for concurrent calls.
    model: Arc<Mutex<TextRerank>>,
    model_id: String,
}

impl FastEmbedReranker {
    pub fn new() -> Result<Self> {
        let opts = RerankInitOptions::new(RerankerModel::BGERerankerBase)
            .with_show_download_progress(false);
        let model = TextRerank::try_new(opts)
            .context("loading BGE-reranker-base (downloads ~110MB on first run)")?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            model_id: DEFAULT_RERANKER_ID.into(),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[async_trait::async_trait]
impl RerankProvider for FastEmbedReranker {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> CoreResult<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let model = self.model.clone();
        let query_owned = query.to_string();
        let docs: Vec<String> = candidates.iter().map(|s| s.to_string()).collect();
        let n = docs.len();
        let join = tokio::task::spawn_blocking(move || {
            let guard = model.blocking_lock();
            // return_documents=false (we only need scores+indices),
            // batch_size=None (use fastembed's default).
            guard.rerank(query_owned.as_str(), docs.iter().map(|s| s.as_str()).collect(), false, None)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?;
        let results = join.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))?;
        Ok(align_by_index(results, n))
    }
}

/// Reorder fastembed's score-descending `Vec<RerankResult>` so the output
/// `Vec<f32>` aligns positionally with the caller's `candidates` slice
/// (i.e. `out[i]` is the score for the original `candidates[i]`).
///
/// Out-of-range indices and missing positions are dropped / left as 0.0
/// respectively; under normal fastembed behavior the result set is a
/// permutation of `0..n` so neither path triggers in production.
fn align_by_index(results: Vec<fastembed::RerankResult>, n: usize) -> Vec<f32> {
    let mut out = vec![0.0_f32; n];
    for r in results {
        if r.index < n {
            out[r.index] = r.score;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::embed::RerankProvider;
    use ohara_core::EmbeddingProvider;

    #[tokio::test]
    #[ignore = "downloads ~80MB on first run; opt-in via `cargo test -- --include-ignored`"]
    async fn embeds_returns_correct_dimension_and_count() {
        let p = FastEmbedProvider::new().unwrap();
        let texts = vec!["hello".to_string(), "retry with backoff".to_string()];
        let out = p.embed_batch(&texts).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), p.dimension());
        assert!(out[0].iter().any(|&x| x != 0.0));
    }

    fn rr(index: usize, score: f32) -> fastembed::RerankResult {
        fastembed::RerankResult {
            document: None,
            score,
            index,
        }
    }

    #[test]
    fn align_by_index_restores_input_order() {
        // fastembed returns results sorted by score desc; original input order is 0,1,2.
        let results = vec![rr(1, 9.0), rr(2, 5.0), rr(0, 1.0)];
        assert_eq!(align_by_index(results, 3), vec![1.0, 9.0, 5.0]);
    }

    #[test]
    fn align_by_index_pads_missing_positions_with_zero() {
        // A truncating reranker (top-k) might omit some indices; remaining
        // positions stay at 0.0 so callers can still index by position.
        let results = vec![rr(2, 7.5), rr(0, 3.0)];
        assert_eq!(align_by_index(results, 4), vec![3.0, 0.0, 7.5, 0.0]);
    }

    #[test]
    fn align_by_index_drops_out_of_range_indices() {
        // Defensive: an index >= n must not panic the caller; just drop it.
        let results = vec![rr(0, 1.0), rr(5, 9.9)];
        assert_eq!(align_by_index(results, 2), vec![1.0, 0.0]);
    }

    #[test]
    fn align_by_index_empty_results_returns_zero_vec() {
        assert_eq!(align_by_index(vec![], 3), vec![0.0, 0.0, 0.0]);
    }

    #[tokio::test]
    #[ignore = "downloads ~110MB on first run; opt-in via `cargo test -- --include-ignored`"]
    async fn reranker_orders_relevant_doc_first() {
        let r = FastEmbedReranker::new().unwrap();
        let candidates = [
            "unrelated cooking recipe",
            "retry helper with exponential backoff",
            "delete user",
        ];
        let scores = r
            .rerank("how to retry on transient failures", &candidates)
            .await
            .unwrap();
        assert_eq!(scores.len(), candidates.len());
        // The retry doc (index 1) must beat both neighbours.
        assert!(
            scores[1] > scores[0],
            "retry doc should outscore unrelated cooking: {scores:?}"
        );
        assert!(
            scores[1] > scores[2],
            "retry doc should outscore delete user: {scores:?}"
        );
    }
}
