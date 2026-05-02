//! BGE-small-en-v1.5 (384d) embedding provider over fastembed-rs,
//! plus BGE-reranker-base cross-encoder over `fastembed::TextRerank`.
//!
//! Concurrency: both `embed_batch` and `rerank` offload the ONNX
//! forward pass to `tokio::task::spawn_blocking` and serialize access
//! to the model via `tokio::sync::Mutex` (see field comments for
//! rationale).

use anyhow::{anyhow, Context, Result};
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

/// ONNX execution provider selector for the embedder + reranker.
///
/// CoreML and CUDA are gated behind cargo features (`coreml` and
/// `cuda` respectively). Building the binary without the feature and
/// then asking for that provider returns an actionable error naming
/// the build flag. The CLI surface (`--embed-provider {auto,cpu,
/// coreml,cuda}`) stays stable across builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmbedProvider {
    #[default]
    Cpu,
    CoreMl,
    Cuda,
}

pub struct FastEmbedProvider {
    // Mutex serializes access: fastembed 4.9 holds mutable tokenizer/batch
    // state inside `embed(&self, ...)` and concurrent calls are not audited.
    model: Arc<Mutex<TextEmbedding>>,
    model_id: String,
    dim: usize,
}

impl FastEmbedProvider {
    /// Backward-compatible default constructor: CPU execution provider.
    /// New call sites should prefer [`FastEmbedProvider::with_provider`].
    pub fn new() -> Result<Self> {
        Self::with_provider(EmbedProvider::Cpu)
    }

    /// Load BGE-small with the requested ONNX execution provider.
    ///
    /// CoreML / CUDA are gated behind cargo features; without the
    /// feature, the corresponding arm returns an actionable error.
    pub fn with_provider(provider: EmbedProvider) -> Result<Self> {
        let opts =
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false);
        let opts = apply_provider_to_init(opts, provider)?;
        let model = TextEmbedding::try_new(opts)
            .context("loading BGE-small model (downloads ~80MB on first run)")?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            model_id: DEFAULT_MODEL_ID.into(),
            dim: DEFAULT_DIM,
        })
    }
}

/// Build the list of `ExecutionProviderDispatch`es to attach for the
/// requested provider. Empty Vec = use ort's default CPU provider.
/// Cargo-feature-gated: building without the relevant feature errors at
/// the boundary with a message naming the missing build flag.
fn execution_providers_for(
    provider: EmbedProvider,
) -> Result<Vec<fastembed::ExecutionProviderDispatch>> {
    match provider {
        EmbedProvider::Cpu => Ok(vec![]),
        EmbedProvider::CoreMl => {
            // CoreML EP requires both the `coreml` cargo feature AND a macOS
            // target — the `ort/coreml` feature only compiles on macOS, and
            // cargo-dist's workspace-wide `features = ["coreml"]` is enabled
            // for non-macOS targets too (where `ohara-embed`'s target-conditional
            // ort dep strips the inner `coreml` feature, so the EP type isn't
            // in scope). Both legs of the gate are needed.
            #[cfg(all(feature = "coreml", target_os = "macos"))]
            {
                use ort::execution_providers::CoreMLExecutionProvider;
                Ok(vec![CoreMLExecutionProvider::default().build()])
            }
            #[cfg(not(all(feature = "coreml", target_os = "macos")))]
            Err(anyhow!(
                "embed-provider=coreml is not enabled in this build. \
                 Rebuild with `cargo build --release --features ohara-embed/coreml` \
                 (Apple Silicon only — pulls in CoreML.framework at link time)."
            ))
        }
        EmbedProvider::Cuda => {
            #[cfg(feature = "cuda")]
            {
                use ort::execution_providers::CUDAExecutionProvider;
                Ok(vec![CUDAExecutionProvider::default().build()])
            }
            #[cfg(not(feature = "cuda"))]
            Err(anyhow!(
                "embed-provider=cuda is not enabled in this build. \
                 Rebuild with `cargo build --release --features ohara-embed/cuda` \
                 (Linux x86_64 with NVIDIA GPU + CUDA toolkit at link time)."
            ))
        }
    }
}

fn apply_provider_to_init(opts: InitOptions, provider: EmbedProvider) -> Result<InitOptions> {
    let eps = execution_providers_for(provider)?;
    if eps.is_empty() {
        Ok(opts)
    } else {
        Ok(opts.with_execution_providers(eps))
    }
}

fn apply_provider_to_rerank(
    opts: RerankInitOptions,
    provider: EmbedProvider,
) -> Result<RerankInitOptions> {
    let eps = execution_providers_for(provider)?;
    if eps.is_empty() {
        Ok(opts)
    } else {
        Ok(opts.with_execution_providers(eps))
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
    /// Backward-compatible default constructor: CPU execution provider.
    /// New call sites should prefer [`FastEmbedReranker::with_provider`].
    pub fn new() -> Result<Self> {
        Self::with_provider(EmbedProvider::Cpu)
    }

    /// Load BGE-reranker-base with the requested ONNX execution provider.
    /// Mirrors [`FastEmbedProvider::with_provider`].
    pub fn with_provider(provider: EmbedProvider) -> Result<Self> {
        let opts = RerankInitOptions::new(RerankerModel::BGERerankerBase)
            .with_show_download_progress(false);
        let opts = apply_provider_to_rerank(opts, provider)?;
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
            guard.rerank(
                query_owned.as_str(),
                docs.iter().map(|s| s.as_str()).collect(),
                false,
                None,
            )
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

    #[test]
    fn provider_cpu_always_returns_empty_provider_list() {
        // CPU = "use ort's default" = empty Vec, no extra providers.
        let eps = execution_providers_for(EmbedProvider::Cpu).expect("cpu always supported");
        assert!(eps.is_empty(), "CPU should not attach explicit providers");
    }

    #[cfg(not(all(feature = "coreml", target_os = "macos")))]
    #[test]
    fn provider_coreml_without_feature_returns_actionable_message() {
        let err = execution_providers_for(EmbedProvider::CoreMl)
            .expect_err("coreml requires --features coreml");
        let s = err.to_string();
        assert!(s.contains("coreml"), "error should name the provider: {s}");
        assert!(
            s.contains("--features"),
            "error should name the build flag: {s}"
        );
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn provider_cuda_without_feature_returns_actionable_message() {
        let err = execution_providers_for(EmbedProvider::Cuda)
            .expect_err("cuda requires --features cuda");
        let s = err.to_string();
        assert!(s.contains("cuda"), "error should name the provider: {s}");
        assert!(
            s.contains("--features"),
            "error should name the build flag: {s}"
        );
    }

    #[cfg(all(feature = "coreml", target_os = "macos"))]
    #[test]
    fn provider_coreml_with_feature_attaches_provider() {
        // With the `coreml` feature on, the provider list is non-empty.
        let eps = execution_providers_for(EmbedProvider::CoreMl)
            .expect("coreml supported with feature on");
        assert_eq!(eps.len(), 1, "CoreML should attach exactly one provider");
    }

    #[test]
    fn embed_provider_default_is_cpu() {
        // Documenting the contract: the CLI's `--embed-provider auto`
        // resolution layer falls back to `EmbedProvider::default()` for
        // unrecognized hosts, so the default must stay CPU.
        assert_eq!(EmbedProvider::default(), EmbedProvider::Cpu);
    }

    // ── CoreML cfg-gate regression tests (PR: both legs of the gate required) ──

    /// On any non-macOS host the CoreML provider must always return an error,
    /// regardless of whether the `coreml` cargo feature is enabled.
    ///
    /// This is the primary regression guard for the PR change from
    ///   `#[cfg(feature = "coreml")]`
    /// to
    ///   `#[cfg(all(feature = "coreml", target_os = "macos"))]`
    ///
    /// Before the PR, passing `--features coreml` on Linux would attempt to
    /// instantiate a CoreML EP that the non-macOS ort dep never provides, leading
    /// to a link-time or runtime failure. After the PR both legs of the gate are
    /// required, so Linux always hits the error arm regardless of the feature.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn provider_coreml_on_non_macos_always_errors_regardless_of_feature() {
        let err = execution_providers_for(EmbedProvider::CoreMl)
            .expect_err("CoreML must error on non-macOS regardless of the coreml feature flag");
        let s = err.to_string();
        assert!(
            s.contains("coreml"),
            "error should name the provider: {s}"
        );
        assert!(
            s.contains("--features"),
            "error should mention the build flag: {s}"
        );
    }

    /// The CoreML error message must guide users to "Apple Silicon only" so they
    /// know the feature is hardware-bound and not just a missing flag.
    #[cfg(not(all(feature = "coreml", target_os = "macos")))]
    #[test]
    fn provider_coreml_error_mentions_apple_silicon() {
        let err = execution_providers_for(EmbedProvider::CoreMl)
            .expect_err("coreml should error without macOS + coreml feature");
        let s = err.to_string();
        assert!(
            s.contains("Apple Silicon"),
            "error should mention Apple Silicon so users know this is hardware-bound: {s}"
        );
    }

    /// The CoreML error message must mention "CoreML.framework" so users
    /// understand the link-time dependency they need on macOS.
    #[cfg(not(all(feature = "coreml", target_os = "macos")))]
    #[test]
    fn provider_coreml_error_mentions_framework_dependency() {
        let err = execution_providers_for(EmbedProvider::CoreMl)
            .expect_err("coreml should error without macOS + coreml feature");
        let s = err.to_string();
        assert!(
            s.contains("CoreML.framework"),
            "error should mention CoreML.framework as the link-time dep: {s}"
        );
    }

    /// `EmbedProvider` must satisfy Copy + Clone + PartialEq + Eq + Debug.
    /// These are all derived in the PR-touched code; verifying them here
    /// catches accidental removal of the derives.
    #[test]
    fn embed_provider_satisfies_copy_clone_partialeq_eq_debug() {
        let original = EmbedProvider::CoreMl;
        let cloned = original.clone();
        let copied: EmbedProvider = original;
        assert_eq!(cloned, copied);
        assert_eq!(original, EmbedProvider::CoreMl);
        assert_ne!(original, EmbedProvider::Cpu);
        assert_ne!(original, EmbedProvider::Cuda);
        // Debug should not panic
        let _ = format!("{original:?}");
    }

    /// All three `EmbedProvider` variants must be distinct so that the match
    /// arms in `execution_providers_for` are exhaustive and non-overlapping.
    #[test]
    fn embed_provider_variants_are_distinct() {
        assert_ne!(EmbedProvider::Cpu, EmbedProvider::CoreMl);
        assert_ne!(EmbedProvider::Cpu, EmbedProvider::Cuda);
        assert_ne!(EmbedProvider::CoreMl, EmbedProvider::Cuda);
    }

    #[tokio::test]
    #[ignore = "downloads ~110MB on first run; opt-in via `cargo test -- --include-ignored`"]
    async fn reranker_orders_relevant_doc_first() {

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
