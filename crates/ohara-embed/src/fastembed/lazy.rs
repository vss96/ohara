//! Lazy `EmbeddingProvider` / `RerankProvider` wrappers (issue #80 split).
//!
//! These defer the ONNX model load until first use so long-lived processes
//! (`ohara serve`, `ohara-mcp`) that never query — or only call
//! `explain_change` — pay nothing. The eager providers they wrap live in
//! [`super`]; load serialization lives in [`crate::idle_slot`].

use super::{
    EmbedProvider, FastEmbedProvider, FastEmbedReranker, DEFAULT_DIM, DEFAULT_MODEL_ID,
    DEFAULT_RERANKER_ID,
};
use ohara_core::embed::RerankProvider;
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Arc;

/// Lazy wrapper around [`FastEmbedReranker`]: defers loading the
/// ~110 MB BGE-reranker-base ONNX session until the first
/// [`RerankProvider::rerank`] call.
///
/// Both `ohara-mcp` and `ohara serve` start long-lived processes that
/// may receive zero `find_pattern` calls, or may receive only
/// `no_rerank: true` calls (which short-circuit the reranker via the
/// retriever's `no_rerank` filter). Eagerly loading the model at
/// startup paid the cold-init cost on every boot — issue #58.
///
/// Load is serialized by a write lock inside [`crate::idle_slot::IdleSlot`];
/// concurrent first callers funnel through a single blocking init.
/// Failed inits leave the slot empty so the next call retries
/// (matching the behaviour of a constructor that would have failed at
/// startup). Plan-29: the session can be dropped after a quiet period
/// via [`RerankProvider::unload_if_idle`] and transparently reloads on
/// next use.
///
/// Init failures are surfaced through [`ohara_core::OhraError::Embedding`]
/// because the [`RerankProvider`] trait can't return `anyhow::Error`.
pub struct LazyFastEmbedReranker {
    slot: crate::idle_slot::IdleSlot<Arc<FastEmbedReranker>>,
    provider: EmbedProvider,
}

impl LazyFastEmbedReranker {
    /// Create a lazy reranker that will load with the CPU execution
    /// provider on first use. Mirrors [`FastEmbedReranker::new`].
    pub fn new() -> Self {
        Self::with_provider(EmbedProvider::Cpu)
    }

    /// Create a lazy reranker that will load with the requested
    /// execution provider on first use.
    pub fn with_provider(provider: EmbedProvider) -> Self {
        Self {
            slot: crate::idle_slot::IdleSlot::new(),
            provider,
        }
    }

    /// Stable id of the model that will be loaded on first use. Safe
    /// to call before initialization — does not trigger a load.
    pub fn model_id(&self) -> &'static str {
        DEFAULT_RERANKER_ID
    }

    async fn get_or_init(&self) -> CoreResult<Arc<FastEmbedReranker>> {
        let provider = self.provider;
        self.slot
            .get_or_try_init(async move {
                tokio::task::spawn_blocking(move || FastEmbedReranker::with_provider(provider))
                    .await
                    .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?
                    .map(Arc::new)
                    .map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
            })
            .await
    }
}

impl Default for LazyFastEmbedReranker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl RerankProvider for LazyFastEmbedReranker {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> CoreResult<Vec<f32>> {
        // Short-circuit before init so an empty-candidates call (the
        // retriever's "no candidates survived RRF" path) never pays
        // the ~110 MB load cost.
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        self.get_or_init().await?.rerank(query, candidates).await
    }

    async fn unload_if_idle(&self, idle: std::time::Duration) -> bool {
        self.slot.unload_if_idle(idle).await
    }
}

/// Lazy wrapper around [`FastEmbedProvider`]: defers loading the
/// BGE-small ONNX session until the first `embed_batch` call.
///
/// Plan-29: the `ohara-mcp` in-process fallback uses this so MCP
/// sessions that never call `find_pattern` (or only call
/// `explain_change`, which needs no models) never pay the embedder
/// load. Identity methods answer from compile-time constants.
pub struct LazyFastEmbedProvider {
    slot: crate::idle_slot::IdleSlot<Arc<FastEmbedProvider>>,
    provider: EmbedProvider,
}

impl LazyFastEmbedProvider {
    /// Create a lazy embedder that will load with the CPU execution
    /// provider on first use. Mirrors [`FastEmbedProvider::new`].
    pub fn new() -> Self {
        Self::with_provider(EmbedProvider::Cpu)
    }

    /// Create a lazy embedder that will load with the requested
    /// execution provider on first use.
    pub fn with_provider(provider: EmbedProvider) -> Self {
        Self {
            slot: crate::idle_slot::IdleSlot::new(),
            provider,
        }
    }

    async fn get_or_init(&self) -> CoreResult<Arc<FastEmbedProvider>> {
        let provider = self.provider;
        self.slot
            .get_or_try_init(async move {
                tokio::task::spawn_blocking(move || FastEmbedProvider::with_provider(provider))
                    .await
                    .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?
                    .map(Arc::new)
                    .map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
            })
            .await
    }
}

impl Default for LazyFastEmbedProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for LazyFastEmbedProvider {
    fn dimension(&self) -> usize {
        DEFAULT_DIM
    }

    fn model_id(&self) -> &str {
        DEFAULT_MODEL_ID
    }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        self.get_or_init().await?.embed_batch(texts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lazy_reranker_empty_candidates_does_not_load_model() {
        // Regression: the empty-candidates short-circuit in
        // `LazyFastEmbedReranker::rerank` is the entire performance claim
        // of issue #58 — without it, `IdleSlot::get_or_try_init` would
        // fire on the first query (even with zero survivors after RRF) and
        // pay the ~110 MB cold-init cost.  The observable consequence is
        // that after the short-circuit call the slot must still be empty,
        // which `unload_if_idle(Duration::ZERO)` detects: it returns
        // `false` both before and after the empty rerank (nothing was ever
        // loaded to unload).
        use ohara_core::embed::RerankProvider as _;
        let lazy = LazyFastEmbedReranker::new();
        assert!(
            !lazy.unload_if_idle(std::time::Duration::ZERO).await,
            "freshly-constructed lazy reranker must not have loaded the model"
        );

        let scores = lazy
            .rerank("any query", &[])
            .await
            .expect("empty rerank must succeed without loading the model");
        assert!(scores.is_empty(), "empty input must yield empty output");

        assert!(
            !lazy.unload_if_idle(std::time::Duration::ZERO).await,
            "rerank(_, &[]) must short-circuit BEFORE get_or_try_init — \
             slot should still be empty"
        );
    }

    #[tokio::test]
    async fn lazy_reranker_unload_without_load_is_false() {
        use ohara_core::embed::RerankProvider as _;
        let r = LazyFastEmbedReranker::new();
        assert!(
            !r.unload_if_idle(std::time::Duration::ZERO).await,
            "nothing loaded yet — nothing to unload"
        );
    }

    #[tokio::test]
    async fn lazy_reranker_empty_candidates_never_loads_or_stamps() {
        use ohara_core::embed::RerankProvider as _;
        let r = LazyFastEmbedReranker::new();
        let scores = r.rerank("q", &[]).await.expect("empty rerank");
        assert!(scores.is_empty());
        // Still nothing to unload: the empty-candidates short-circuit
        // must not have touched the slot.
        assert!(!r.unload_if_idle(std::time::Duration::ZERO).await);
    }

    #[test]
    fn lazy_embedder_reports_identity_without_init() {
        use ohara_core::EmbeddingProvider as _;
        let e = LazyFastEmbedProvider::new();
        // Both must answer from constants — constructing the provider
        // and asking for identity must never load the ONNX session.
        assert_eq!(e.model_id(), DEFAULT_MODEL_ID);
        assert_eq!(e.dimension(), DEFAULT_DIM);
    }
}
