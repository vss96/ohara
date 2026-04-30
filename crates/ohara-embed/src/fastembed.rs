use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5";
const DEFAULT_DIM: usize = 384;

pub struct FastEmbedProvider {
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
        let opts = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(false);
        let model = TextEmbedding::try_new(opts)?;
        Ok(Self { model: Arc::new(Mutex::new(model)), model_id: DEFAULT_MODEL_ID.into(), dim: DEFAULT_DIM })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn dimension(&self) -> usize { self.dim }
    fn model_id(&self) -> &str { &self.model_id }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() { return Ok(vec![]); }
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
