use crate::Result;
use async_trait::async_trait;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dimension(&self) -> usize;

    fn model_id(&self) -> &str;

    /// Embed a batch of texts. The output has the same length and order as the input.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}
