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
    EmbeddingModel, InitOptions, OnnxSource, RerankInitOptions, RerankInitOptionsUserDefined,
    RerankerModel, TextEmbedding, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};
use ohara_core::embed::RerankProvider;
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Arc;
use tokio::sync::Mutex;

mod lazy;
pub use lazy::{LazyFastEmbedProvider, LazyFastEmbedReranker};

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
/// Model id of the opt-in INT8 cross-encoder (issue #55). The reranker is
/// a query-time component (no rerank artifacts are persisted), so swapping
/// it does NOT invalidate an existing index — unlike the embedder id.
pub const INT8_RERANKER_ID: &str = "bge-reranker-base-int8";
/// Hugging Face repo + file for the INT8 reranker. A Transformers.js-style
/// export of `BAAI/bge-reranker-base`; `model_int8.onnx` is ~280 MB vs the
/// 1.1 GB full-precision model.
const INT8_RERANKER_REPO: &str = "onnx-community/bge-reranker-base-ONNX";
const INT8_RERANKER_ONNX_FILE: &str = "onnx/model_int8.onnx";

/// Which cross-encoder reranker to load (issue #55).
///
/// `Base` is the full-precision built-in `bge-reranker-base` (default).
/// `BaseInt8` is the opt-in INT8 variant, selected via the `OHARA_RERANKER`
/// environment variable. The default flip stays gated on a recall eval
/// (`tests/perf/context_engine_eval.rs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RerankerChoice {
    #[default]
    Base,
    BaseInt8,
}

impl RerankerChoice {
    /// Resolve from the `OHARA_RERANKER` opt-in. Unset or any unrecognized
    /// value falls back to the full-precision default.
    pub fn from_env() -> Self {
        Self::from_opt(std::env::var("OHARA_RERANKER").ok().as_deref())
    }

    /// Pure mapping from an optional flag value to a choice (testable
    /// without touching the process environment).
    fn from_opt(value: Option<&str>) -> Self {
        match value {
            Some("bge-reranker-base-int8") | Some("int8") => RerankerChoice::BaseInt8,
            _ => RerankerChoice::Base,
        }
    }

    /// Stable model id for this choice — answerable without loading.
    pub fn model_id(self) -> &'static str {
        match self {
            RerankerChoice::Base => DEFAULT_RERANKER_ID,
            RerankerChoice::BaseInt8 => INT8_RERANKER_ID,
        }
    }
}

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
        let opts = InitOptions::new(EmbeddingModel::BGESmallENV15Q)
            .with_show_download_progress(false)
            .with_cache_dir(crate::cache::cache_dir());
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

    /// Load BGE-reranker-base with the requested ONNX execution provider,
    /// honoring the `OHARA_RERANKER` opt-in. Mirrors
    /// [`FastEmbedProvider::with_provider`].
    pub fn with_provider(provider: EmbedProvider) -> Result<Self> {
        Self::with_provider_and_choice(provider, RerankerChoice::from_env())
    }

    /// Load the reranker for an explicit [`RerankerChoice`] (issue #55).
    ///
    /// `Base` uses fastembed's built-in full-precision model. `BaseInt8`
    /// downloads the INT8 export from `onnx-community/bge-reranker-base-ONNX`
    /// and loads it via fastembed's user-defined-model path.
    pub fn with_provider_and_choice(
        provider: EmbedProvider,
        choice: RerankerChoice,
    ) -> Result<Self> {
        match choice {
            RerankerChoice::Base => {
                let opts = RerankInitOptions::new(RerankerModel::BGERerankerBase)
                    .with_show_download_progress(false)
                    .with_cache_dir(crate::cache::cache_dir());
                let opts = apply_provider_to_rerank(opts, provider)?;
                let model = TextRerank::try_new(opts)
                    .context("loading BGE-reranker-base (downloads ~110MB on first run)")?;
                Ok(Self {
                    model: Arc::new(Mutex::new(model)),
                    model_id: DEFAULT_RERANKER_ID.into(),
                })
            }
            RerankerChoice::BaseInt8 => {
                let user_model = download_int8_reranker()?;
                // Seed max_length (512) from the built-in options, then
                // attach the chosen execution providers. The model itself
                // comes from the user-defined ONNX, not the enum.
                let mut opts: RerankInitOptionsUserDefined =
                    RerankInitOptions::new(RerankerModel::BGERerankerBase).into();
                opts.execution_providers = execution_providers_for(provider)?;
                let model = TextRerank::try_new_from_user_defined(user_model, opts)
                    .context("loading INT8 bge-reranker-base (downloads ~280MB on first run)")?;
                Ok(Self {
                    model: Arc::new(Mutex::new(model)),
                    model_id: INT8_RERANKER_ID.into(),
                })
            }
        }
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Download the INT8 `bge-reranker-base` ONNX + tokenizer files from the
/// Hugging Face hub and assemble a [`UserDefinedRerankingModel`] (issue #55).
/// Uses the same sync `hf-hub` API and on-disk cache fastembed uses for its
/// built-in models, so a previously-fetched model is reused.
fn download_int8_reranker() -> Result<UserDefinedRerankingModel> {
    use hf_hub::api::sync::ApiBuilder;
    let api = ApiBuilder::new()
        .with_progress(false)
        .build()
        .context("init hf-hub api for the INT8 reranker")?;
    let repo = api.model(INT8_RERANKER_REPO.to_string());
    let onnx_path = repo
        .get(INT8_RERANKER_ONNX_FILE)
        .with_context(|| format!("download {INT8_RERANKER_ONNX_FILE} from {INT8_RERANKER_REPO}"))?;
    let read = |name: &str| -> Result<Vec<u8>> {
        let path = repo
            .get(name)
            .with_context(|| format!("download {name} from {INT8_RERANKER_REPO}"))?;
        std::fs::read(&path).with_context(|| format!("read {}", path.display()))
    };
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    Ok(UserDefinedRerankingModel::new(
        OnnxSource::File(onnx_path),
        tokenizer_files,
    ))
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
    fn reranker_choice_defaults_to_full_precision() {
        // Issue #55: the INT8 reranker is opt-in. Unset or unrecognized
        // OHARA_RERANKER values MUST resolve to the full-precision default
        // so existing behavior is unchanged.
        assert_eq!(RerankerChoice::default(), RerankerChoice::Base);
        assert_eq!(RerankerChoice::from_opt(None), RerankerChoice::Base);
        assert_eq!(RerankerChoice::from_opt(Some("")), RerankerChoice::Base);
        assert_eq!(
            RerankerChoice::from_opt(Some("bogus")),
            RerankerChoice::Base
        );
    }

    #[test]
    fn reranker_choice_int8_opt_in_values() {
        assert_eq!(
            RerankerChoice::from_opt(Some("int8")),
            RerankerChoice::BaseInt8
        );
        assert_eq!(
            RerankerChoice::from_opt(Some("bge-reranker-base-int8")),
            RerankerChoice::BaseInt8
        );
    }

    #[test]
    fn reranker_choice_model_ids_are_distinct() {
        assert_eq!(RerankerChoice::Base.model_id(), DEFAULT_RERANKER_ID);
        assert_eq!(RerankerChoice::BaseInt8.model_id(), INT8_RERANKER_ID);
        assert_ne!(
            RerankerChoice::Base.model_id(),
            RerankerChoice::BaseInt8.model_id()
        );
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
