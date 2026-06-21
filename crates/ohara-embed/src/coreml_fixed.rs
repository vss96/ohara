//! Fixed-shape CoreML embedding provider for Apple Silicon (plan-30).
//!
//! `ohara index --embed-provider coreml` routes here. The provider runs
//! the full-precision BGE-small (`bge-small-en-v1.5`, same 384d space as
//! the quantized default — measured cross-model parity min cosine
//! 1.0000) with its graph dims baked to `(32, 512)` via
//! [`crate::onnx_dims`], under the CoreML EP in MLProgram format with
//! all compute units. On an M4 Pro this embeds at ~118 rows/s vs ~39
//! for the INT8-CPU default (3.05×) with a flat memory footprint; see
//! `docs/perf/v0.11-coreml-fixed-shape.md` for the investigation and
//! why every other CoreML configuration fails.
//!
//! Shape discipline is what makes this safe: CoreML retains a compiled
//! specialization per distinct tensor shape (the plan-7 "leak"), and
//! onnxruntime's converter emits ANE-rejected unbounded dims unless the
//! graph itself is fixed. Three layers enforce one shape:
//! 1. the model graph is patched to `(32, 512)`;
//! 2. the tokenizer pads to `Fixed(512)` (fastembed's default
//!    pad-to-longest would produce shorter rows on short batches);
//! 3. `embed_batch` splits into 32-row chunks and pads the tail with
//!    empty strings, truncating the output back.
//!
//! The reranker never uses CoreML, and the daemon never loads this
//! provider — indexing is always a foreground CLI pass.

use anyhow::Result;
use ohara_core::{EmbeddingProvider, Result as CoreResult};

/// Rows per ONNX call — baked into the patched model's batch dim.
/// 8 and 32 measured within noise of each other (119.0 vs 118.5 rows/s);
/// 32 amortizes per-call overhead on large index passes.
pub const FIXED_BATCH: usize = 32;
/// Token cap per row — BGE-small's maximum; also the baked seq dim.
pub const FIXED_SEQ: usize = 512;
/// Stable identity of the full-precision model. Equivalent to the
/// quantized `bge-small-en-v1.5-q` for compatibility purposes (the two
/// form an equivalence class in `CompatibilityStatus::assess`).
pub const FP32_MODEL_ID: &str = "bge-small-en-v1.5";

/// Pad a chunk of rows up to `width` with empty strings so every ONNX
/// call carries exactly the baked batch dimension. Callers truncate the
/// returned vectors back to `chunk.len()`.
fn pad_rows(chunk: &[String], width: usize) -> Vec<String> {
    let mut rows = chunk.to_vec();
    rows.resize(width, String::new());
    rows
}

/// `EmbeddingProvider` backed by the shape-fixed fp32 BGE-small under
/// CoreML. Construction is gated on the `coreml` cargo feature + macOS;
/// other builds get an actionable error naming the build flag.
pub struct CoreMlFixedProvider {
    model: std::sync::Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>,
}

impl CoreMlFixedProvider {
    /// Load (downloading the ~130MB fp32 model on first use), patch the
    /// graph dims in memory, and compile the CoreML session (~30s, once
    /// per process).
    pub fn new() -> Result<Self> {
        let model = build_model()?;
        Ok(Self {
            model: std::sync::Arc::new(tokio::sync::Mutex::new(model)),
        })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for CoreMlFixedProvider {
    fn dimension(&self) -> usize {
        crate::DEFAULT_DIM
    }
    fn model_id(&self) -> &str {
        FP32_MODEL_ID
    }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = self.model.clone();
        let owned: Vec<String> = texts.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            let mut out: Vec<Vec<f32>> = Vec::with_capacity(owned.len());
            for chunk in owned.chunks(FIXED_BATCH) {
                let padded = pad_rows(chunk, FIXED_BATCH);
                let refs: Vec<&str> = padded.iter().map(|s| s.as_str()).collect();
                let vecs = guard.embed(refs, None)?;
                out.extend(vecs.into_iter().take(chunk.len()));
            }
            Ok::<_, anyhow::Error>(out)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?;
        result.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
    }
}

#[cfg(all(feature = "coreml", target_os = "macos"))]
fn build_model() -> Result<fastembed::TextEmbedding> {
    imp::build_model()
}

#[cfg(not(all(feature = "coreml", target_os = "macos")))]
fn build_model() -> Result<fastembed::TextEmbedding> {
    anyhow::bail!(
        "embed-provider=coreml is not enabled in this build. \
         Rebuild with `cargo build --release --features ohara-embed/coreml` \
         (Apple Silicon only — pulls in CoreML.framework at link time)."
    )
}

/// On-disk location for ORT's compiled-CoreML model cache, kept beside
/// the downloaded model snapshots (under [`crate::cache::cache_dir`]) so
/// it persists across processes. Handed to the CoreML EP via
/// `with_model_cache_dir`, this lets the ~30s MLProgram compile be paid
/// once and reused, instead of recompiled on every `ohara index` run.
#[cfg(any(all(feature = "coreml", target_os = "macos"), test))]
fn coreml_cache_subdir(cache_root: &std::path::Path) -> std::path::PathBuf {
    cache_root.join("ohara-coreml-compiled")
}

#[cfg(all(feature = "coreml", target_os = "macos"))]
mod imp {
    use super::{FIXED_BATCH, FIXED_SEQ};
    use anyhow::{anyhow, Context, Result};
    use fastembed::{
        EmbeddingModel, InitOptions, InitOptionsUserDefined, Pooling, TextEmbedding,
        TokenizerFiles, UserDefinedEmbeddingModel,
    };
    use std::path::PathBuf;

    /// Hugging Face repo dir (hf-hub cache layout) for the fp32 model.
    const FP32_REPO_DIR: &str = "models--Xenova--bge-small-en-v1.5";
    const REQUIRED_FILES: &[&str] = &[
        "onnx/model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ];

    pub(super) fn build_model() -> Result<TextEmbedding> {
        use ort::ep::coreml::{ComputeUnits, ModelFormat};
        use ort::ep::CoreML;

        let snap = ensure_fp32_snapshot()?;
        let read = |rel: &str| {
            std::fs::read(snap.join(rel))
                .with_context(|| format!("reading {rel} from fp32 snapshot {}", snap.display()))
        };
        let onnx = read("onnx/model.onnx")?;
        let patched = crate::onnx_dims::fix_graph_io_dims(
            &onnx,
            &[
                ("batch_size", FIXED_BATCH as u64),
                ("sequence_length", FIXED_SEQ as u64),
            ],
        )
        .context("fixing graph dims on bge-small fp32")?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let udm =
            UserDefinedEmbeddingModel::new(patched, tokenizer_files).with_pooling(Pooling::Cls);
        // Persist the compiled MLProgram so the ~30s compile is paid once
        // per machine, not once per process. ORT keys the cache on graph
        // structure; our graph is the same pinned model patched to the
        // same `(32, 512)` dims every run, so subsequent runs hit the
        // cache and load the precompiled model instead of recompiling.
        //
        // Best-effort: caching is an optimization, so if the cache dir
        // can't be created (read-only/shared cache root) we log and
        // compile without it — the prior behavior — rather than failing
        // the whole index pass.
        let coreml = CoreML::default()
            .with_model_format(ModelFormat::MLProgram)
            .with_compute_units(ComputeUnits::All);
        let coreml = match ensure_coreml_cache_dir() {
            Ok(cache_dir) => coreml.with_model_cache_dir(cache_dir.to_string_lossy()),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "CoreML compile cache unavailable; continuing without persistent cache"
                );
                coreml
            }
        };
        let eps = vec![coreml.build()];
        let opts = InitOptionsUserDefined::new()
            .with_execution_providers(eps)
            .with_max_length(FIXED_SEQ);
        let mut model = TextEmbedding::try_new_from_user_defined(udm, opts)
            .context("compiling the shape-fixed CoreML session (MLProgram)")?;
        // fastembed hardcodes pad-to-longest; the fixed graph needs every
        // row at exactly FIXED_SEQ. Pad token/id are BGE constants
        // ([PAD] = 0, from the model's tokenizer_config/config).
        let _ = model
            .tokenizer
            .with_padding(Some(tokenizers::PaddingParams {
                strategy: tokenizers::PaddingStrategy::Fixed(FIXED_SEQ),
                pad_token: "[PAD]".into(),
                pad_id: 0,
                ..Default::default()
            }));
        Ok(model)
    }

    /// Resolve and create the compiled-CoreML cache directory under the
    /// fastembed cache root, returning the path to hand to the CoreML EP's
    /// `with_model_cache_dir`. ORT requires the directory to exist.
    fn ensure_coreml_cache_dir() -> Result<PathBuf> {
        let dir = super::coreml_cache_subdir(&crate::cache::cache_dir());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating CoreML compile cache at {}", dir.display()))?;
        Ok(dir)
    }

    /// Locate the fp32 snapshot in the fastembed cache, downloading it
    /// via fastembed's own machinery (construct + drop a CPU embedder)
    /// when missing. Keeps the cache layout identical to the default
    /// embedder path — no extra HTTP dependency.
    fn ensure_fp32_snapshot() -> Result<PathBuf> {
        if let Some(dir) = find_complete_snapshot()? {
            return Ok(dir);
        }
        tracing::info!("downloading fp32 bge-small-en-v1.5 (~130MB, one time)");
        let opts = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(false)
            .with_cache_dir(crate::cache::cache_dir());
        drop(
            TextEmbedding::try_new(opts)
                .context("downloading fp32 BGE-small (~130MB on first use)")?,
        );
        find_complete_snapshot()?.ok_or_else(|| {
            anyhow!(
                "fp32 snapshot not found under {} after download",
                snapshots_root().display()
            )
        })
    }

    fn snapshots_root() -> PathBuf {
        crate::cache::cache_dir()
            .join(FP32_REPO_DIR)
            .join("snapshots")
    }

    fn find_complete_snapshot() -> Result<Option<PathBuf>> {
        let root = snapshots_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => return Ok(None),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if REQUIRED_FILES.iter().all(|f| dir.join(f).exists()) {
                return Ok(Some(dir));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_rows_fills_short_chunks_with_empty_strings() {
        let chunk = vec!["a".to_string(), "b".to_string()];
        let padded = pad_rows(&chunk, 4);
        assert_eq!(padded.len(), 4);
        assert_eq!(padded[0], "a");
        assert_eq!(padded[1], "b");
        assert_eq!(padded[2], "");
        assert_eq!(padded[3], "");
    }

    #[test]
    fn pad_rows_leaves_full_chunks_unchanged() {
        let chunk: Vec<String> = (0..4).map(|i| format!("row {i}")).collect();
        let padded = pad_rows(&chunk, 4);
        assert_eq!(padded, chunk);
    }

    #[test]
    fn coreml_cache_subdir_sits_beside_the_model_cache() {
        // The compiled-CoreML cache MUST live at a stable, model-cache-
        // relative location so a warm cache survives across `ohara index`
        // runs (moving it would silently force the ~30s recompile again).
        let root = std::path::Path::new("/tmp/fe-cache");
        assert_eq!(
            coreml_cache_subdir(root),
            std::path::Path::new("/tmp/fe-cache/ohara-coreml-compiled"),
        );
    }

    #[cfg(not(all(feature = "coreml", target_os = "macos")))]
    #[test]
    fn constructor_names_the_build_flag_without_the_feature() {
        let err = match CoreMlFixedProvider::new() {
            Ok(_) => panic!("constructor must fail without the coreml feature"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("ohara-embed/coreml"),
            "error must name the build flag, got: {err}"
        );
    }

    /// Hardware-bound end-to-end: requires the coreml build on Apple
    /// Silicon and downloads the fp32 model on first run.
    ///
    /// ```sh
    /// cargo test -p ohara-embed --release --features coreml \
    ///     -- --ignored coreml_fixed --nocapture
    /// ```
    #[cfg(all(feature = "coreml", target_os = "macos"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "hardware-bound: compiles a CoreML session, downloads ~130MB on first run"]
    async fn coreml_fixed_embeds_mixed_length_batches() {
        let provider = CoreMlFixedProvider::new().expect("construct CoreML fixed provider");
        assert_eq!(provider.model_id(), "bge-small-en-v1.5");
        assert_eq!(provider.dimension(), 384);

        // 40 rows = one full 32-chunk plus a padded 8-row tail; lengths
        // straddle short and 512-token-saturating.
        let texts: Vec<String> = (0..40)
            .map(|i| {
                let unit =
                    "fn lookup(&self, key: u64) -> Option<&Page> { self.frames.get(&key) }\n";
                let reps = match i % 3 {
                    0 => 1,
                    1 => 20,
                    _ => 80,
                };
                format!("[row {i}] {}", unit.repeat(reps))
            })
            .collect();
        let vecs = provider.embed_batch(&texts).await.expect("embed");
        assert_eq!(vecs.len(), 40, "tail padding must be truncated away");
        for v in &vecs {
            assert_eq!(v.len(), 384);
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-3,
                "vectors must be l2-normalized, got {norm}"
            );
        }
        let cos: f32 = vecs[0].iter().zip(&vecs[1]).map(|(a, b)| a * b).sum();
        assert!(cos < 0.999, "distinct rows must not collapse to one vector");
    }
}
