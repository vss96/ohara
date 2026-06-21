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
    embed_mode: crate::EmbedMode,
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

        // Plan 27: compute embedder input per hunk based on mode.
        //   Off / Semantic → effective_semantic_text
        //   Diff           → record.diff_text  (drops commit message)
        let mode = self.embed_mode;
        let hunk_inputs: Vec<String> = attributed_hunks
            .iter()
            .map(|ah| match mode {
                crate::EmbedMode::Diff => ah.record.diff_text.clone(),
                _ => ah.effective_semantic_text().to_owned(),
            })
            .collect();

        // Cache lookup (mode != Off and cache is wired).
        let model_id = self.embedder.model_id().to_owned();
        let cached: std::collections::HashMap<crate::types::ContentHash, Vec<f32>> =
            match (mode, self.cache.as_ref()) {
                (crate::EmbedMode::Off, _) | (_, None) => std::collections::HashMap::new(),
                (_, Some(cache)) => {
                    let hashes: Vec<crate::types::ContentHash> = hunk_inputs
                        .iter()
                        .map(|s| crate::types::ContentHash::from_text(s))
                        .collect();
                    cache.embed_cache_get_many(&hashes, &model_id).await?
                }
            };

        // Build the text batch: commit message at index 0, then only
        // the hunk inputs that missed the cache.  Deduplicate within
        // the batch by hash so identical inputs (e.g. same diff_text in
        // Diff mode) are embedded only once per commit.
        let mut batch_texts: Vec<String> = Vec::with_capacity(hunk_inputs.len() + 1);
        batch_texts.push(commit_message.to_owned());
        // Maps content-hash → batch index (1-based, after commit msg).
        let mut hash_to_batch_idx: std::collections::HashMap<crate::types::ContentHash, usize> =
            std::collections::HashMap::new();
        for input in &hunk_inputs {
            let hash = crate::types::ContentHash::from_text(input);
            if cached.contains_key(&hash) || hash_to_batch_idx.contains_key(&hash) {
                continue;
            }
            let idx = batch_texts.len(); // position in batch_texts
            hash_to_batch_idx.insert(hash, idx);
            batch_texts.push(input.clone());
        }

        // Embed the batch (commit message + unique misses).
        let all_embs = self.embed_in_chunks(&batch_texts).await?;
        let (commit_vec, miss_vecs) = all_embs.split_first().ok_or_else(|| {
            OhraError::Embedding("embed_batch returned empty for non-empty input".into())
        })?;
        if miss_vecs.len() != hash_to_batch_idx.len() {
            return Err(OhraError::Embedding(format!(
                "miss vector count {} != unique miss count {}",
                miss_vecs.len(),
                hash_to_batch_idx.len()
            )));
        }

        // Write misses back to the cache.
        if mode != crate::EmbedMode::Off {
            if let Some(cache) = self.cache.as_ref() {
                let entries: Vec<(crate::types::ContentHash, Vec<f32>)> = hash_to_batch_idx
                    .iter()
                    .map(|(hash, &batch_idx)| {
                        // batch_idx is 1-based (0 = commit msg), so subtract 1 for miss_vecs index.
                        (hash.clone(), miss_vecs[batch_idx - 1].clone())
                    })
                    .collect();
                cache.embed_cache_put_many(&entries, &model_id).await?;
            }
        }

        // Assemble final EmbeddedHunk list in original input order.
        // For each hunk, resolve its embedding from: cache hit, or
        // the batch result (looked up by content-hash → batch_idx).
        let hunks: Vec<EmbeddedHunk> = attributed_hunks
            .iter()
            .zip(hunk_inputs.iter())
            .map(|(ah, input)| {
                let hash = crate::types::ContentHash::from_text(input);
                let embedding = match cached.get(&hash) {
                    Some(v) => v.clone(),
                    None => {
                        let batch_idx = hash_to_batch_idx
                            .get(&hash)
                            .expect("invariant: every miss hash has a batch index");
                        miss_vecs[batch_idx - 1].clone()
                    }
                };
                EmbeddedHunk {
                    attributed: ah.clone(),
                    embedding,
                }
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

    // InMemoryCacheStorage lives in test_helpers to keep this file ≤ 500 lines.
    use super::super::test_helpers::InMemoryCacheStorage;

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

    /// Embedder fake that records the texts it received per call. Used
    /// to assert that `Diff` mode changes the embedder input.
    struct RecordingEmbedder {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for RecordingEmbedder {
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            "recorder"
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.seen.lock().unwrap().push(texts.to_vec());
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    fn attributed_with_diff(diff: &str, semantic: &str) -> AttributedHunk {
        AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: diff.into(),
                semantic_text: semantic.into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }
    }

    #[tokio::test]
    async fn diff_mode_feeds_diff_text_to_embedder_not_semantic_text() {
        // Plan 27 Task C.3: in Diff mode the embedder receives
        // diff_text, not the commit-message-prefixed semantic_text.
        // The cache key is sha256(diff_text), so two hunks with
        // identical diff_text but different semantic_text produce a
        // single embed call for the second hunk.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let embedder = Arc::new(RecordingEmbedder {
            seen: seen.clone(),
            dim: 4,
        });
        // Use the InMemoryCacheStorage from test_helpers (added in C.2).
        let cache: Arc<dyn crate::Storage> =
            Arc::new(super::super::test_helpers::InMemoryCacheStorage::default());
        let stage = EmbedStage::new(embedder.clone())
            .with_embed_mode(crate::EmbedMode::Diff)
            .with_cache(cache.clone());

        // Two hunks: identical diff_text, distinct semantic_text.
        let hunks = vec![
            attributed_with_diff("+let x = 1;\n", "msg one\n\n+let x = 1;\n"),
            attributed_with_diff("+let x = 1;\n", "msg two\n\n+let x = 1;\n"),
        ];
        let _ = stage.run("commit msg", &hunks).await.unwrap();

        // The first call should contain commit_msg + the diff_text
        // ONCE (the second hunk's diff_text matches the first → cache
        // hit → not in batch).
        let calls = seen.lock().unwrap().clone();
        let total_seen: Vec<&String> = calls.iter().flatten().collect();
        let diff_count = total_seen
            .iter()
            .filter(|s| s.contains("+let x = 1;"))
            .count();
        assert_eq!(
            diff_count, 1,
            "Diff mode should embed identical diff_text only once, got {diff_count}: {calls:?}"
        );

        // The embedder must NOT have seen the prefixed semantic_text
        // in Diff mode.
        let saw_semantic = total_seen
            .iter()
            .any(|s| s.contains("msg one") || s.contains("msg two"));
        assert!(
            !saw_semantic,
            "Diff mode should not feed semantic_text to embedder: {calls:?}"
        );
    }

    #[tokio::test]
    async fn diff_mode_symbol_attributed_hunk_embeds_same_as_unattributed() {
        // Issue #100 claim (a) regression guard. Two hunks share an
        // identical `diff_text`; one carries symbol attribution
        // (`attributed_semantic_text = Some(...)` — the prefix the
        // attribute stage prepends), the other does not. In Diff mode
        // the embedder input is `diff_text` only, so BOTH hunks MUST
        // produce the same vector — the symbol signal is intentionally
        // dropped from the Diff vector lane (see `EmbedMode::Diff`
        // docs: "embedder input is `diff_text` only"). This documents
        // the trade-off, not a bug: Diff mode is a distinct vector lane
        // that requires `--rebuild` to switch into.
        let dim = 6;
        // Deterministic embedder: vector is derived from input length so
        // identical inputs map to identical vectors and distinct inputs
        // would differ — lets us assert "same vector" meaningfully.
        struct LenEmbedder {
            dim: usize,
        }
        #[async_trait]
        impl EmbeddingProvider for LenEmbedder {
            fn dimension(&self) -> usize {
                self.dim
            }
            fn model_id(&self) -> &str {
                "len"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts
                    .iter()
                    .map(|t| vec![t.len() as f32; self.dim])
                    .collect())
            }
        }

        let attributed_hunk = AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: "+    self.retry();\n".into(),
                semantic_text: "+    self.retry();\n".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            // The symbol prefix the attribute stage would add. In
            // Semantic/Off mode this changes the vector; in Diff mode
            // it must NOT, because diff_text is the embedder input.
            attributed_semantic_text: Some("retry_with_backoff\n+    self.retry();\n".into()),
        };
        let unattributed_hunk = AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "g.rs".into(),
                diff_text: "+    self.retry();\n".into(),
                semantic_text: "+    self.retry();\n".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        };

        let embedder = Arc::new(LenEmbedder { dim });
        let stage = EmbedStage::new(embedder).with_embed_mode(crate::EmbedMode::Diff);
        let out = stage
            .run("commit msg", &[attributed_hunk, unattributed_hunk])
            .await
            .unwrap();
        assert_eq!(out.hunks.len(), 2);
        assert_eq!(
            out.hunks[0].embedding, out.hunks[1].embedding,
            "Diff mode must embed identical diff_text identically regardless of symbol attribution"
        );
    }

    #[tokio::test]
    async fn semantic_mode_symbol_attribution_changes_the_vector() {
        // Issue #100 claim (a) contrast guard. The same two hunks from
        // `diff_mode_symbol_attributed_hunk_embeds_same_as_unattributed`
        // (identical diff_text, one with a symbol prefix) must produce
        // DIFFERENT vectors in Semantic/Off mode, because there the
        // embedder input is `effective_semantic_text` — which includes
        // the prepended symbol. Together the two tests show the symbol
        // signal is dropped ONLY in Diff mode, by design.
        let dim = 6;
        struct LenEmbedder {
            dim: usize,
        }
        #[async_trait]
        impl EmbeddingProvider for LenEmbedder {
            fn dimension(&self) -> usize {
                self.dim
            }
            fn model_id(&self) -> &str {
                "len"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts
                    .iter()
                    .map(|t| vec![t.len() as f32; self.dim])
                    .collect())
            }
        }

        let attributed_hunk = AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: "+    self.retry();\n".into(),
                semantic_text: "+    self.retry();\n".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: Some("retry_with_backoff\n+    self.retry();\n".into()),
        };
        let unattributed_hunk = AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "g.rs".into(),
                diff_text: "+    self.retry();\n".into(),
                semantic_text: "+    self.retry();\n".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        };

        let embedder = Arc::new(LenEmbedder { dim });
        // Off mode (no cache) exercises the same effective_semantic_text
        // path as Semantic without needing cache wiring.
        let stage = EmbedStage::new(embedder);
        let out = stage
            .run("commit msg", &[attributed_hunk, unattributed_hunk])
            .await
            .unwrap();
        assert_eq!(out.hunks.len(), 2);
        assert_ne!(
            out.hunks[0].embedding, out.hunks[1].embedding,
            "Semantic/Off mode must reflect the symbol prefix in the vector"
        );
    }

    #[tokio::test]
    async fn within_batch_identical_inputs_embedded_once_and_share_vector() {
        // Regression guard for the per-hunk hash dedup (issue #110): two
        // hunks with identical embedder input in a single run, with NO
        // cache, must be embedded only once (the second is deduped by
        // content hash within the batch) and both output hunks must
        // receive the same vector. This pins the within-batch dedup
        // branch (`hash_to_batch_idx.contains_key`) that the cache-based
        // tests do not exercise.
        let dim = 5;
        // Deterministic embedder: vector derived from input length so an
        // accidental double-embed of distinct text would change the
        // observed call count, and identical inputs map to equal vectors.
        struct LenEmbedder {
            calls: Arc<Mutex<Vec<Vec<String>>>>,
            dim: usize,
        }
        #[async_trait]
        impl EmbeddingProvider for LenEmbedder {
            fn dimension(&self) -> usize {
                self.dim
            }
            fn model_id(&self) -> &str {
                "len"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.calls.lock().unwrap().push(texts.to_vec());
                Ok(texts
                    .iter()
                    .map(|t| vec![t.len() as f32; self.dim])
                    .collect())
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let embedder = Arc::new(LenEmbedder {
            calls: calls.clone(),
            dim,
        });
        // Off mode, no cache: the only dedup path is the within-batch
        // hash map. Two hunks with the same semantic_text.
        let hunks = vec![attributed("same body"), attributed("same body")];
        let stage = EmbedStage::new(embedder);
        let out = stage.run("commit msg", &hunks).await.unwrap();

        // Commit message + the deduped body = 2 distinct texts total.
        let seen: Vec<String> = calls.lock().unwrap().iter().flatten().cloned().collect();
        let body_count = seen.iter().filter(|s| s.as_str() == "same body").count();
        assert_eq!(
            body_count, 1,
            "identical inputs in one batch must be embedded once, got {body_count}: {seen:?}"
        );
        assert_eq!(out.hunks.len(), 2);
        assert_eq!(
            out.hunks[0].embedding, out.hunks[1].embedding,
            "both hunks sharing one input must resolve to the same vector"
        );
    }

    #[tokio::test]
    async fn semantic_mode_second_run_reuses_cached_vectors_and_skips_embed() {
        // Plan 27 Task C.2: with EmbedMode::Semantic + a cache, the
        // first call embeds normally and writes to the cache; the
        // second call with the same hunks must hit the cache and call
        // embed_batch only for the commit message.
        let calls = Arc::new(Mutex::new(Vec::<usize>::new()));
        let embedder = Arc::new(CountingEmbedder {
            calls: calls.clone(),
            dim: 4,
        });
        let cache: Arc<dyn crate::Storage> = Arc::new(InMemoryCacheStorage::default());
        let stage = EmbedStage::new(embedder.clone())
            .with_embed_mode(crate::EmbedMode::Semantic)
            .with_cache(cache.clone());

        let hunks = vec![attributed("hunk one"), attributed("hunk two")];
        let _ = stage.run("commit msg", &hunks).await.unwrap();

        // First run: 1 commit message + 2 hunks = 3 inputs.
        let observed = calls.lock().unwrap().clone();
        let total_first: usize = observed.iter().sum();
        assert_eq!(total_first, 3, "first run must embed 3 texts: {observed:?}");

        // Second run with identical hunks: only the commit message
        // should be embedded (commit messages are not cached). The
        // two hunks must be served from cache.
        let _ = stage.run("commit msg", &hunks).await.unwrap();
        let after = calls.lock().unwrap().clone();
        let total_second: usize = after.iter().sum::<usize>() - total_first;
        assert_eq!(
            total_second, 1,
            "second run must embed only 1 text (commit msg): added {total_second} after first run"
        );
    }
}
