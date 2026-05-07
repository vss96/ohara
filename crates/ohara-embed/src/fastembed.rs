//! BGE-small-en-v1.5 quantized (384d) embedding provider over
//! fastembed-rs, plus BGE-reranker-base cross-encoder over
//! `fastembed::TextRerank`.
//!
//! Issue #54: the default embedder is the INT8-quantized variant
//! (`Qdrant/bge-small-en-v1.5-onnx-Q`, exposed as
//! `EmbeddingModel::BGESmallENV15Q` in fastembed). Same 384d output as
//! the full-precision model, ~1.5–3× CPU throughput, ~50% lower memory
//! footprint, and the recall delta on the retrieval-quality fixture is
//! within tolerance (`tests/perf/context_engine_eval.rs`). The model id
//! `"bge-small-en-v1.5-q"` carries the `-q` suffix so an index built
//! with the older binary is reported as `compatibility: needs rebuild`.
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
use tokio::sync::{Mutex, OnceCell};

/// Stable id of the default embedder model. Mirrored on every
/// `FastEmbedProvider::model_id()` and recorded in `index_metadata`
/// (plan 13) so an old index built with a different model triggers a
/// rebuild prompt.
///
/// Issue #54: switched from `"bge-small-en-v1.5"` (full precision) to
/// the quantized variant. The `-q` suffix is part of the index identity
/// — old indexes built with the full-precision embedder produce vectors
/// that are not directly comparable to Q-variant query embeddings, so
/// the suffix forces `CompatibilityStatus::assess` to return
/// `NeedsRebuild` after binary upgrade.
pub const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5-q";
/// Vector dimension produced by `DEFAULT_MODEL_ID`. Exposed so the
/// `ohara status` command can build the runtime compatibility
/// expectation without loading the embedder (plan 13 Task 3.1).
///
/// Both the full-precision and quantized BGE-small variants emit 384d
/// vectors, so the dimension is stable across the #54 switch.
pub const DEFAULT_DIM: usize = 384;
/// Stable id of the default cross-encoder reranker model. Recorded in
/// `index_metadata` so a reranker swap triggers a refresh prompt.
pub const DEFAULT_RERANKER_ID: &str = "bge-reranker-base";

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
    // Mutex serializes access: fastembed 5.x's `embed(&mut self, ...)`
    // signature requires exclusive access to the model session, and the
    // tokenizer/batch state is not audited for concurrent use either way.
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

    /// Load BGE-small (quantized) with the requested ONNX execution
    /// provider.
    ///
    /// CoreML / CUDA are gated behind cargo features; without the
    /// feature, the corresponding arm returns an actionable error. The
    /// quantized model file (`Qdrant/bge-small-en-v1.5-onnx-Q`) is
    /// downloaded on first run; size is comparable to the full-precision
    /// model since the optimized ONNX export is ~33MB.
    pub fn with_provider(provider: EmbedProvider) -> Result<Self> {
        let opts =
            InitOptions::new(EmbeddingModel::BGESmallENV15Q).with_show_download_progress(false);
        let opts = apply_provider_to_init(opts, provider)?;
        let model = TextEmbedding::try_new(opts)
            .context("loading BGE-small (quantized) model (downloads ~33MB on first run)")?;
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
            Err(anyhow::anyhow!(
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
            Err(anyhow::anyhow!(
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
            // fastembed 5.x's `TextEmbedding::embed` is `&mut self`; the
            // Mutex's blocking guard derefs to `&mut TextEmbedding` so a
            // single `mut` binding is enough. Tokio's `Mutex` is fair
            // and we only ever hold the guard for one batch, so callers
            // get FIFO access on contention.
            let mut guard = model.blocking_lock();
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
    // fastembed 5.x's `rerank(&mut self, ...)` requires exclusive
    // access to the model session, and the underlying tokenizer state
    // is not audited for concurrent use either way.
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
            // fastembed 5.x's `TextRerank::rerank` is `&mut self`. See
            // FastEmbedProvider::embed_batch for the same pattern.
            let mut guard = model.blocking_lock();
            // return_documents=false (we only need scores+indices),
            // batch_size=None (use fastembed's default).
            //
            // Annotated as `Vec<&str>` because fastembed 5.x's `rerank`
            // takes `impl AsRef<[S]>` where S is inferred from the query
            // and document slice independently — the inner `.collect()`
            // needs a concrete type to satisfy that bound.
            let doc_refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
            guard.rerank(query_owned.as_str(), doc_refs, false, None)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?;
        let results = join.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))?;
        Ok(align_by_index(results, n))
    }
}

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
/// First-call init uses [`tokio::sync::OnceCell`] so concurrent first
/// callers serialize on a single load; subsequent calls bypass the
/// cell entirely and dispatch straight into the inner reranker.
///
/// Init failures are surfaced through [`ohara_core::OhraError::Embedding`]
/// because the [`RerankProvider`] trait can't return `anyhow::Error`.
/// `OnceCell::get_or_try_init` only stores the success value, so a
/// failed first init is retried on the next call (which matches the
/// behavior of an eagerly-constructed reranker that would have failed
/// at startup).
pub struct LazyFastEmbedReranker {
    cell: OnceCell<FastEmbedReranker>,
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
            cell: OnceCell::new(),
            provider,
        }
    }

    /// Stable id of the model that will be loaded on first use. Safe
    /// to call before initialization — does not trigger a load.
    pub fn model_id(&self) -> &'static str {
        DEFAULT_RERANKER_ID
    }

    async fn get_or_init(&self) -> CoreResult<&FastEmbedReranker> {
        let provider = self.provider;
        self.cell
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || FastEmbedReranker::with_provider(provider))
                    .await
                    .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?
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

    #[test]
    fn default_model_id_pins_quantized_variant() {
        // Issue #54: the default embedder is the quantized BGE-small
        // variant. The model id MUST be distinct from the full-precision
        // `"bge-small-en-v1.5"` so that an index built with the old
        // binary is detected as `NeedsRebuild` after upgrade (vector
        // geometry differs between the two models).
        assert_eq!(DEFAULT_MODEL_ID, "bge-small-en-v1.5-q");
        assert_ne!(
            DEFAULT_MODEL_ID, "bge-small-en-v1.5",
            "Q variant must not share the full-precision model id"
        );
        // Dimension is unchanged (384 for both variants), but pin it so
        // a future model swap that changes dim updates this test in
        // lockstep with `RuntimeIndexMetadata`.
        assert_eq!(DEFAULT_DIM, 384);
    }

    #[tokio::test]
    async fn lazy_reranker_empty_candidates_does_not_load_model() {
        // Regression: the empty-candidates short-circuit in
        // `LazyFastEmbedReranker::rerank` is the entire performance claim
        // of issue #58 — without it, `OnceCell::get_or_init` would fire on
        // the first query (even with zero survivors after RRF) and pay the
        // ~110 MB cold-init cost. If someone reorders the empty-check to
        // run after `get_or_init`, the inner `OnceCell` will transition to
        // initialized and this assertion will fail.
        //
        // Strict-distinguisher check: mentally swap the two lines in
        // `rerank` so `get_or_init().await?` runs before the
        // `candidates.is_empty()` guard — the cell would be populated and
        // `cell.get()` would return `Some`, failing the assertion below.
        let lazy = LazyFastEmbedReranker::new();
        assert!(
            lazy.cell.get().is_none(),
            "freshly-constructed lazy reranker must not have loaded the model"
        );

        let scores = lazy
            .rerank("any query", &[])
            .await
            .expect("empty rerank must succeed without loading the model");
        assert!(scores.is_empty(), "empty input must yield empty output");

        assert!(
            lazy.cell.get().is_none(),
            "rerank(_, &[]) must short-circuit BEFORE get_or_init — \
             OnceCell should still be uninitialized"
        );
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
