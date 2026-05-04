//! Output type and stage implementation for the embed stage.

use super::attribute::AttributedHunk;

/// An `AttributedHunk` extended with its embedding vector, produced by
/// the embed stage.
#[derive(Debug, Clone)]
pub struct EmbeddedHunk {
    /// The upstream attributed hunk.
    pub attributed: AttributedHunk,
    /// Embedding vector for this hunk's effective semantic text.
    pub embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::stages::{
        attribute::AttributedHunk, hunk_chunk::HunkRecord,
    };
    use crate::embed::EmbeddingProvider;
    use crate::types::Hunk;
    use crate::{OhraError, Result};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    fn attributed(text: &str) -> AttributedHunk {
        AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: "+x\n".into(),
                semantic_text: text.into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }
    }

    struct CountingEmbedder {
        calls: Arc<Mutex<Vec<usize>>>,
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            "counter"
        }
        async fn embed_batch(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>> {
            self.calls.lock().unwrap().push(texts.len());
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    #[tokio::test]
    async fn with_embed_batch_2_produces_correct_chunk_count() {
        // 6 hunks + 1 commit message = 7 texts.
        // with_embed_batch(2) → chunks of [2, 2, 2, 1] = 4 calls.
        let hunks: Vec<AttributedHunk> = (0..6).map(|i| attributed(&format!("h{i}"))).collect();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let embedder = Arc::new(CountingEmbedder {
            calls: calls.clone(),
            dim: 4,
        });
        let stage = EmbedStage::new(embedder).with_embed_batch(2);
        let result = stage.run("commit message", &hunks).await.unwrap();
        assert_eq!(result.hunks.len(), 6, "must produce one EmbeddedHunk per input");
        let observed = calls.lock().unwrap().clone();
        // 7 texts / 2 = chunks [2, 2, 2, 1]
        assert_eq!(
            observed,
            vec![2, 2, 2, 1],
            "embed_batch(2) on 7 texts must produce 4 calls, got {observed:?}"
        );
        for &sz in &observed {
            assert!(sz <= 2, "chunk size {sz} exceeded knob");
        }
    }

    #[tokio::test]
    async fn empty_hunk_list_yields_empty_output() {
        let embedder = Arc::new(CountingEmbedder {
            calls: Arc::new(Mutex::new(vec![])),
            dim: 4,
        });
        let stage = EmbedStage::new(embedder);
        let result = stage.run("msg", &[]).await.unwrap();
        assert!(result.hunks.is_empty());
    }

    #[tokio::test]
    async fn embed_vectors_have_correct_dimension() {
        let dim = 8;
        let hunks = vec![attributed("foo"), attributed("bar")];
        let embedder = Arc::new(CountingEmbedder {
            calls: Arc::new(Mutex::new(vec![])),
            dim,
        });
        let stage = EmbedStage::new(embedder);
        let result = stage.run("msg", &hunks).await.unwrap();
        for eh in &result.hunks {
            assert_eq!(
                eh.embedding.len(),
                dim,
                "embedding must have dimension {dim}"
            );
        }
    }
}
