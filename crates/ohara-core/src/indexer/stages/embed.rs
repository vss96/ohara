//! Output type and stage implementation for the embed stage.

use super::attribute::AttributedHunk;
use crate::embed::EmbeddingProvider;
use crate::{OhraError, Result};
use std::sync::Arc;

/// The embed stage: calls `EmbeddingProvider::embed_batch` in chunks
/// of at most `embed_batch` texts, concatenates the results, and
/// returns one `EmbeddedHunk` per input `AttributedHunk`.
///
/// The commit-message embedding is produced in the same batch (as
/// element 0) and returned via `commit_embedding` in `EmbedOutput`
/// so the coordinator can store it alongside the commit row.
///
/// This is the only stage that holds a constructor-time configuration
/// value (`embed_batch`). Default: 32.
pub struct EmbedStage {
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    #[allow(dead_code)]
    embed_mode: crate::EmbedMode,
    #[allow(dead_code)]
    cache: Option<Arc<dyn crate::Storage>>,
}

/// Output of the embed stage for a single commit.
pub struct EmbedOutput {
    /// Embedding vector for the commit message (element 0 of the full
    /// text batch).
    pub commit_embedding: Vec<f32>,
    /// One `EmbeddedHunk` per input `AttributedHunk`, in the same
    /// order.
    pub hunks: Vec<EmbeddedHunk>,
}

impl EmbedStage {
    /// Construct a new embed stage wrapping `embedder` with the
    /// default `embed_batch` of 32.
    pub fn new(embedder: Arc<dyn EmbeddingProvider + Send + Sync>) -> Self {
        Self {
            embedder,
            embed_batch: 32,
            embed_mode: crate::EmbedMode::default(),
            cache: None,
        }
    }

    /// Override the per-commit embed batch size. `0` is normalised to
    /// `1`. Smaller values cap peak allocator pressure at the cost of
    /// more `embed_batch` calls per commit.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Set the embed mode. Off (default) means no cache lookups;
    /// Semantic / Diff turn on the chunk-embed cache and (for Diff)
    /// change the embedder input. Plan 27.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        self
    }

    /// Wire a `Storage` impl that backs the chunk embed cache. Only
    /// consulted when `with_embed_mode` is set to Semantic or Diff.
    /// Plan 27.
    pub fn with_cache(mut self, storage: Arc<dyn crate::Storage>) -> Self {
        self.cache = Some(storage);
        self
    }

    /// Run the embed stage for a single commit.
    ///
    /// `commit_message` is placed at index 0 of the text batch;
    /// `attributed_hunks[i].effective_semantic_text()` occupies indices
    /// 1..=n. The returned `EmbedOutput::hunks` is in the same order as
    /// `attributed_hunks`.
    pub async fn run(
        &self,
        commit_message: &str,
        attributed_hunks: &[AttributedHunk],
    ) -> Result<EmbedOutput> {
        if attributed_hunks.is_empty() {
            // Still embed the commit message alone.
            let embs = self
                .embedder
                .embed_batch(&[commit_message.to_owned()])
                .await?;
            let commit_embedding = embs
                .into_iter()
                .next()
                .ok_or_else(|| OhraError::Embedding("embed_batch returned empty".into()))?;
            return Ok(EmbedOutput {
                commit_embedding,
                hunks: vec![],
            });
        }

        // Build the full text list: commit message first, then hunks.
        let mut texts: Vec<String> = Vec::with_capacity(attributed_hunks.len() + 1);
        texts.push(commit_message.to_owned());
        for ah in attributed_hunks {
            texts.push(ah.effective_semantic_text().to_owned());
        }

        // Chunked embedding (plan-15 knob).
        let all_embs = self.embed_in_chunks(&texts).await?;

        let (commit_vec, hunk_vecs) = all_embs.split_first().ok_or_else(|| {
            OhraError::Embedding("embed_batch returned empty for non-empty input".into())
        })?;

        let hunks = attributed_hunks
            .iter()
            .zip(hunk_vecs.iter())
            .map(|(ah, emb)| EmbeddedHunk {
                attributed: ah.clone(),
                embedding: emb.clone(),
            })
            .collect();

        Ok(EmbedOutput {
            commit_embedding: commit_vec.clone(),
            hunks,
        })
    }

    /// Slice `texts` into chunks of `self.embed_batch`, embed each
    /// chunk, and concatenate results. Keeps peak-embed allocation
    /// bounded regardless of commit size.
    async fn embed_in_chunks(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let cap = self.embed_batch.max(1);
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(cap) {
            let chunk_owned: Vec<String> = chunk.to_vec();
            let mut embs = self.embedder.embed_batch(&chunk_owned).await?;
            if embs.len() != chunk_owned.len() {
                return Err(OhraError::Embedding(format!(
                    "embed_batch returned {} vectors for {} inputs",
                    embs.len(),
                    chunk_owned.len()
                )));
            }
            out.append(&mut embs);
        }
        Ok(out)
    }
}

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
    use crate::embed::EmbeddingProvider;
    use crate::indexer::stages::{attribute::AttributedHunk, hunk_chunk::HunkRecord};
    use crate::types::Hunk;
    use crate::Result;
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
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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
        assert_eq!(
            result.hunks.len(),
            6,
            "must produce one EmbeddedHunk per input"
        );
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
