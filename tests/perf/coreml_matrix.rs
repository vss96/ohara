#![cfg(target_os = "macos")]
//! Apple-Silicon CoreML investigation matrix (B2 spike).
//!
//! Measures embed throughput across {INT8, FP32} BGE-small ×
//! {CPU, CoreML-NeuralNetwork, CoreML-MLProgram, CoreML-ANE-only}
//! against a questdb-shaped corpus (length mix sampled from the real
//! `hunk.semantic_text` distribution: p50≈1.2k chars, p90≈5.7k, 33%
//! over 2k — most rows saturate BGE's 512-token cap).
//!
//! Why this exists: `ohara index` on Apple Silicon currently runs the
//! INT8-quantized model on CPU — `--embed-provider auto` downgrades at
//! ≥1000 commits (plan-7 leak mitigation), and even explicit CoreML
//! passes ort's *default* EP config, which is the legacy NeuralNetwork
//! format. The CoreML EP supports no quantized ops at all, and
//! LayerNormalization/Gelu only exist in MLProgram format, so neither
//! half of today's wiring can put BGE matmuls on the ANE/GPU. This
//! harness answers: what does CoreML-done-right actually buy?
//!
//! Hardware-bound and `#[ignore]`'d. Run manually on Apple Silicon:
//!
//! ```sh
//! cargo test --release -p ohara-perf-tests --features coreml \
//!     --test coreml_matrix -- --include-ignored --nocapture
//! ```
//!
//! ## Knobs (all optional environment variables)
//!
//! | Var              | Default | Meaning                                    |
//! | ---------------- | ------- | ------------------------------------------ |
//! | `MATRIX_CONFIGS` | all     | csv of config names (see `Cfg::name`)      |
//! | `MATRIX_CORPUS`  | 1024    | corpus rows per timing iteration           |
//! | `MATRIX_BATCH`   | 128     | strings per `embed` call (mirrors auto)    |
//! | `MATRIX_ITERS`   | 2       | timing iterations (median reported)        |
//! | `MATRIX_SUSTAIN` | 0       | extra leak-probe batches after timing      |

use std::time::Instant;

use fastembed::{
    EmbeddingModel, ExecutionProviderDispatch, InitOptions, InitOptionsUserDefined, Pooling,
    TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

const PROBES: &[&str] = &[
    "fn parse_commit(repo: &Repository, oid: Oid) -> Result<Commit>",
    "retry the request with exponential backoff and jitter",
    "BEGIN IMMEDIATE; INSERT INTO vec_commit(rowid, embedding) VALUES (?,?)",
    "impl EmbeddingProvider for FastEmbedProvider { async fn embed_batch }",
    "fix flaky test by waiting for the writer thread to flush",
    "add LRU cache in front of the blame lookup",
    "migrate the column store to mmap-backed page frames",
    "refactor: extract the wal replay loop into its own module",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cfg {
    /// Today's default on every platform: quantized model, CPU EP.
    Int8Cpu,
    /// Pre-v0.9 default: full-precision model, CPU EP. Parity reference.
    Fp32Cpu,
    /// What `--embed-provider coreml` wires today: ort's default EP
    /// config = legacy NeuralNetwork format.
    Fp32CoremlNn,
    /// CoreML done right: MLProgram format, all compute units.
    Fp32CoremlMlprog,
    /// MLProgram, CPU+ANE only (no GPU) — isolates the ANE.
    Fp32CoremlAne,
    /// Quantized model under MLProgram — expected to fall back to CPU
    /// (CoreML EP supports no quantized ops); confirms the hypothesis.
    Int8CoremlMlprog,
    /// Shape-fixed FP32 model (`make_dynamic_shape_fixed`, batch+seq
    /// baked in) on plain CPU — control for the CoreML variant below.
    /// Requires `MATRIX_FIXED_ONNX` + `MATRIX_UNIFORM=1` and
    /// `MATRIX_BATCH` equal to the model's baked batch dim.
    Fp32CpuFixed,
    /// Shape-fixed FP32 model under CoreML MLProgram — the official
    /// workaround for the unbounded-dimension ANE rejection
    /// (onnxruntime "make dynamic input shape fixed" guidance).
    Fp32CoremlFixed,
}

/// Configs run by default (no extra env required).
const ALL_CONFIGS: &[Cfg] = &[
    Cfg::Int8Cpu,
    Cfg::Fp32Cpu,
    Cfg::Fp32CoremlNn,
    Cfg::Fp32CoremlMlprog,
    Cfg::Fp32CoremlAne,
    Cfg::Int8CoremlMlprog,
];

/// Every selectable config, including the env-gated fixed-shape ones.
const NAMED_CONFIGS: &[Cfg] = &[
    Cfg::Int8Cpu,
    Cfg::Fp32Cpu,
    Cfg::Fp32CoremlNn,
    Cfg::Fp32CoremlMlprog,
    Cfg::Fp32CoremlAne,
    Cfg::Int8CoremlMlprog,
    Cfg::Fp32CpuFixed,
    Cfg::Fp32CoremlFixed,
];

impl Cfg {
    fn name(self) -> &'static str {
        match self {
            Cfg::Int8Cpu => "int8-cpu",
            Cfg::Fp32Cpu => "fp32-cpu",
            Cfg::Fp32CoremlNn => "fp32-coreml-nn",
            Cfg::Fp32CoremlMlprog => "fp32-coreml-mlprog",
            Cfg::Fp32CoremlAne => "fp32-coreml-ane",
            Cfg::Int8CoremlMlprog => "int8-coreml-mlprog",
            Cfg::Fp32CpuFixed => "fp32-cpu-fixed",
            Cfg::Fp32CoremlFixed => "fp32-coreml-fixed",
        }
    }

    fn model(self) -> EmbeddingModel {
        match self {
            Cfg::Int8Cpu | Cfg::Int8CoremlMlprog => EmbeddingModel::BGESmallENV15Q,
            _ => EmbeddingModel::BGESmallENV15,
        }
    }

    fn is_fixed(self) -> bool {
        matches!(self, Cfg::Fp32CpuFixed | Cfg::Fp32CoremlFixed)
    }

    fn eps(self) -> Vec<ExecutionProviderDispatch> {
        use ort::ep::coreml::{ComputeUnits, ModelFormat};
        use ort::ep::CoreML;
        match self {
            Cfg::Int8Cpu | Cfg::Fp32Cpu | Cfg::Fp32CpuFixed => vec![],
            Cfg::Fp32CoremlNn => vec![CoreML::default().build()],
            Cfg::Fp32CoremlMlprog | Cfg::Int8CoremlMlprog | Cfg::Fp32CoremlFixed => {
                vec![CoreML::default()
                    .with_model_format(ModelFormat::MLProgram)
                    .with_compute_units(matrix_units())
                    .build()]
            }
            Cfg::Fp32CoremlAne => vec![CoreML::default()
                .with_model_format(ModelFormat::MLProgram)
                .with_compute_units(ComputeUnits::CPUAndNeuralEngine)
                .build()],
        }
    }

    fn from_name(name: &str) -> Option<Cfg> {
        NAMED_CONFIGS.iter().copied().find(|c| c.name() == name)
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Compute-units override for the `*-coreml-mlprog` configs
/// (`MATRIX_UNITS`): isolates which CoreML backend (BNNS CPU / Metal
/// GPU / ANE) triggers instability.
fn matrix_units() -> ort::ep::coreml::ComputeUnits {
    use ort::ep::coreml::ComputeUnits;
    let raw = std::env::var("MATRIX_UNITS").unwrap_or_else(|_| "all".into());
    match raw.as_str() {
        "all" => ComputeUnits::All,
        "cpugpu" => ComputeUnits::CPUAndGPU,
        "cpuane" => ComputeUnits::CPUAndNeuralEngine,
        "cpuonly" => ComputeUnits::CPUOnly,
        other => panic!("MATRIX_UNITS must be all|cpugpu|cpuane|cpuonly, got {other:?}"),
    }
}

/// `MATRIX_UNIFORM=1` makes every corpus row long enough to saturate
/// the 512-token cap and skips the short-string parity probes, so every
/// tensor CoreML sees is exactly (batch, 512) — the static-shape
/// workaround for onnxruntime#21227-style dynamic-shape crashes.
fn uniform_mode() -> bool {
    env_parse("MATRIX_UNIFORM", 0usize) == 1
}

fn selected_configs() -> Vec<Cfg> {
    let raw = match std::env::var("MATRIX_CONFIGS") {
        Ok(v) => v,
        Err(_) => return ALL_CONFIGS.to_vec(),
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Cfg::from_name(s).unwrap_or_else(|| panic!("unknown MATRIX_CONFIGS entry {s:?}")))
        .collect()
}

/// Current phys_footprint (the number Activity Monitor's "Memory"
/// column shows) — unlike `peak_rss_bytes` this is not monotonic, so
/// deltas across a timing loop approximate net growth.
fn phys_footprint_bytes() -> Option<u64> {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(info.ri_phys_footprint)
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Build a code-flavoured string of roughly `len` bytes. Salted so no
/// two rows are identical (mirrors the real corpus: 98.6% of questdb
/// hunks are unique).
fn make_text(len: usize, salt: usize) -> String {
    let snippets = [
        "fn next_page(&mut self, frame: PageFrame) -> Result<Cursor> {",
        "    if self.wal.is_sealed() { return Err(Error::Sealed); }",
        "    let offset = self.index.lookup(frame.id)?;",
        "SELECT ts, symbol, price FROM trades WHERE ts > dateadd('d', -1, now())",
        "@Override public void onCommit(long txn, int partition) {",
        "    columnVersionWriter.upsert(txn, columnIndex, partition);",
        "let mut writer = self.acquire_writer(table_token).await?;",
        "assertEquals(expected.getQuick(i), actual.getQuick(i));",
    ];
    let mut buf = format!("[hunk {salt}] commit: refactor partition replay path\n");
    let mut i = salt;
    while buf.len() < len {
        buf.push_str(snippets[i % snippets.len()]);
        buf.push('\n');
        i += 1;
    }
    buf.truncate(len);
    buf
}

/// Length mix sampled from the questdb index's `hunk.semantic_text`
/// (chars, weight%). Rows ≥ ~2.5k chars all saturate the 512-token cap.
const LEN_MIX: &[(usize, usize)] = &[(1200, 50), (3000, 25), (5700, 15), (12000, 10)];

fn build_corpus(size: usize) -> Vec<String> {
    if uniform_mode() {
        // Every row ≥512 tokens → tokenizer pads-to-longest gives a
        // constant (batch, 512) shape on every call.
        return (0..size).map(|salt| make_text(4000, salt)).collect();
    }
    let mut corpus = Vec::with_capacity(size);
    let mut salt = 0usize;
    'outer: loop {
        for &(len, weight) in LEN_MIX {
            let rows = (size * weight).div_ceil(100);
            for _ in 0..rows {
                if corpus.len() == size {
                    break 'outer;
                }
                corpus.push(make_text(len, salt));
                salt += 1;
            }
        }
    }
    corpus
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (na * nb)
}

fn load_model(cfg: Cfg) -> TextEmbedding {
    if cfg.is_fixed() {
        return load_fixed_model(cfg);
    }
    let cache = ohara_perf_tests::workspace_root().join(".fastembed_cache");
    let opts = InitOptions::new(cfg.model())
        .with_cache_dir(cache)
        .with_show_download_progress(true)
        .with_execution_providers(cfg.eps());
    TextEmbedding::try_new(opts).unwrap_or_else(|e| panic!("loading model for {}: {e}", cfg.name()))
}

/// Load a shape-fixed BGE-small via fastembed's user-defined-model
/// path. `MATRIX_FIXED_ONNX` points at a model produced by
/// `python -m onnxruntime.tools.make_dynamic_shape_fixed` with both
/// `batch_size` and `sequence_length` baked in; tokenizer files come
/// from the standard fp32 snapshot in the workspace cache. Pooling is
/// CLS to match fastembed's built-in BGE handling.
fn load_fixed_model(cfg: Cfg) -> TextEmbedding {
    assert!(
        uniform_mode(),
        "fixed-shape configs require MATRIX_UNIFORM=1 (every tensor must be (batch, 512))"
    );
    let onnx_path = std::env::var("MATRIX_FIXED_ONNX")
        .expect("MATRIX_FIXED_ONNX must point at a shape-fixed BGE-small onnx");
    let snap_root = ohara_perf_tests::workspace_root()
        .join(".fastembed_cache/models--Xenova--bge-small-en-v1.5/snapshots");
    let snap = std::fs::read_dir(&snap_root)
        .expect("fp32 model snapshot dir (run the fp32-cpu config once to download)")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("at least one snapshot");
    let read =
        |name: &str| std::fs::read(snap.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json"),
        config_file: read("config.json"),
        special_tokens_map_file: read("special_tokens_map.json"),
        tokenizer_config_file: read("tokenizer_config.json"),
    };
    let onnx_file = std::fs::read(&onnx_path)
        .unwrap_or_else(|e| panic!("read MATRIX_FIXED_ONNX {onnx_path}: {e}"));
    let udm = UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files).with_pooling(Pooling::Cls);
    let opts = InitOptionsUserDefined::new()
        .with_execution_providers(cfg.eps())
        .with_max_length(512);
    TextEmbedding::try_new_from_user_defined(udm, opts)
        .unwrap_or_else(|e| panic!("loading fixed model for {}: {e}", cfg.name()))
}

fn embed_all(model: &mut TextEmbedding, corpus: &[String], batch: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::with_capacity(corpus.len());
    let mut i = 0;
    while i < corpus.len() {
        let end = (i + batch).min(corpus.len());
        let refs: Vec<&str> = corpus[i..end].iter().map(|s| s.as_str()).collect();
        let vecs = model.embed(refs, None).expect("embed_batch failed mid-run");
        out.extend(vecs);
        i = end;
    }
    out
}

struct CellResult {
    cfg: Cfg,
    load_ms: u128,
    warm_ms: u128,
    median_ms: u128,
    rps: f64,
    min_cos: f64,
    foot_delta_mb: f64,
    sustain_slope_mb: Option<f64>,
}

#[test]
#[ignore = "hardware-bound perf matrix — opt in via --include-ignored coreml_matrix --nocapture"]
fn coreml_matrix() {
    let corpus_size: usize = env_parse("MATRIX_CORPUS", 1024);
    let batch: usize = env_parse("MATRIX_BATCH", 128);
    let iters: usize = env_parse("MATRIX_ITERS", 2);
    let sustain: usize = env_parse("MATRIX_SUSTAIN", 0);
    let configs = selected_configs();

    eprintln!(
        "coreml_matrix: pid={} corpus={} batch={} iters={} sustain={} configs={:?}",
        std::process::id(),
        corpus_size,
        batch,
        iters,
        sustain,
        configs.iter().map(|c| c.name()).collect::<Vec<_>>(),
    );

    let mut corpus = build_corpus(corpus_size);
    if uniform_mode() {
        // Keep every call at exactly `batch` rows (no short remainder).
        corpus.truncate(corpus.len() / batch * batch);
        assert!(!corpus.is_empty(), "MATRIX_CORPUS must be >= MATRIX_BATCH");
    }
    // Probes: short standard strings normally; in uniform mode the
    // first `batch` corpus rows, so fixed-shape configs only ever see
    // (batch, 512) tensors. The reference model is plain fp32-cpu and
    // handles either shape.
    let probe_owned: Vec<String> = match uniform_mode() {
        true => corpus[..batch.min(corpus.len())].to_vec(),
        false => PROBES.iter().map(|s| s.to_string()).collect(),
    };

    // Parity reference: FP32 on CPU (the numerically-trustworthy path).
    eprintln!("[ref] loading fp32-cpu for parity reference...");
    let mut reference = load_model(Cfg::Fp32Cpu);
    let ref_vecs = embed_all(&mut reference, &probe_owned, batch);
    drop(reference);

    let mut results: Vec<CellResult> = Vec::new();
    for cfg in configs {
        eprintln!("\n=== {} ===", cfg.name());

        let t0 = Instant::now();
        let mut model = load_model(cfg);
        let load_ms = t0.elapsed().as_millis();
        eprintln!(
            "[{}] session created in {}ms, warming up...",
            cfg.name(),
            load_ms
        );

        // First batch separately: CoreML compiles/specializes here.
        let t0 = Instant::now();
        let _ = embed_all(&mut model, &corpus[..batch.min(corpus.len())], batch);
        let warm_ms = t0.elapsed().as_millis();
        eprintln!(
            "[{}] load={}ms first-batch={}ms",
            cfg.name(),
            load_ms,
            warm_ms
        );

        let cand_vecs = embed_all(&mut model, &probe_owned, batch);
        let min_cos = ref_vecs
            .iter()
            .zip(&cand_vecs)
            .map(|(a, b)| cosine(a, b))
            .fold(f64::INFINITY, f64::min);
        eprintln!(
            "[{}] parity vs fp32-cpu: min cosine = {:.4}",
            cfg.name(),
            min_cos
        );

        let foot_before = phys_footprint_bytes().unwrap_or(0);
        let mut samples: Vec<u128> = Vec::with_capacity(iters);
        for it in 0..iters {
            let t0 = Instant::now();
            let vecs = embed_all(&mut model, &corpus, batch);
            let ms = t0.elapsed().as_millis();
            assert_eq!(vecs.len(), corpus.len(), "embed returned wrong row count");
            let rps = corpus.len() as f64 / (ms as f64 / 1000.0);
            eprintln!(
                "[{}] iter {} -> {}ms ({:.1} rows/s)",
                cfg.name(),
                it,
                ms,
                rps
            );
            samples.push(ms);
        }
        let foot_after = phys_footprint_bytes().unwrap_or(0);
        samples.sort_unstable();
        let median_ms = samples[samples.len() / 2];
        let rps = corpus.len() as f64 / (median_ms as f64 / 1000.0);
        let foot_delta_mb = mb(foot_after.saturating_sub(foot_before));

        // Optional sustained-leak probe: fresh salted batches in a tight
        // loop, footprint sampled every 50, least-squares slope reported.
        let mut sustain_slope_mb = None;
        if sustain > 0 {
            let mut points: Vec<(f64, f64)> = Vec::new();
            let sustain_len = match uniform_mode() {
                true => 4000,
                false => 1200,
            };
            for i in 0..sustain {
                let texts: Vec<String> = (0..batch)
                    .map(|j| make_text(sustain_len, 1_000_000 + i * batch + j))
                    .collect();
                let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                model.embed(refs, None).expect("sustain embed failed");
                if i % 50 == 0 {
                    if let Some(f) = phys_footprint_bytes() {
                        points.push((i as f64, mb(f)));
                        eprintln!(
                            "[{}] sustain iter {} footprint={:.1}MB",
                            cfg.name(),
                            i,
                            mb(f)
                        );
                    }
                }
            }
            if points.len() >= 2 {
                let n = points.len() as f64;
                let sx: f64 = points.iter().map(|p| p.0).sum();
                let sy: f64 = points.iter().map(|p| p.1).sum();
                let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
                let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
                let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
                sustain_slope_mb = Some(slope);
                eprintln!("[{}] sustain slope = {:.3} MB/batch", cfg.name(), slope);
            }
        }

        results.push(CellResult {
            cfg,
            load_ms,
            warm_ms,
            median_ms,
            rps,
            min_cos,
            foot_delta_mb,
            sustain_slope_mb,
        });
        drop(model);
    }

    let base_rps = results
        .iter()
        .find(|r| r.cfg == Cfg::Int8Cpu)
        .map(|r| r.rps);
    eprintln!("\n=== summary (corpus={corpus_size}, batch={batch}, median of {iters}) ===");
    eprintln!(
        "{:>20} | {:>8} | {:>9} | {:>9} | {:>8} | {:>9} | {:>8} | {:>9} | {:>9}",
        "config",
        "load_ms",
        "1st_batch",
        "median_ms",
        "rows/s",
        "vs int8",
        "min_cos",
        "footΔ_MB",
        "MB/batch"
    );
    for r in &results {
        let speedup = match base_rps {
            Some(base) if base > 0.0 => format!("{:>6.2}x", r.rps / base),
            _ => "?".to_string(),
        };
        let slope = match r.sustain_slope_mb {
            Some(s) => format!("{s:>8.3}"),
            None => "-".to_string(),
        };
        eprintln!(
            "{:>20} | {:>8} | {:>9} | {:>9} | {:>8.1} | {:>9} | {:>8.4} | {:>9.1} | {:>9}",
            r.cfg.name(),
            r.load_ms,
            r.warm_ms,
            r.median_ms,
            r.rps,
            speedup,
            r.min_cos,
            r.foot_delta_mb,
            slope,
        );
    }
}
