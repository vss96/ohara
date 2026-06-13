# CoreML fixed-shape embedder (plan-30 preview)

**Date:** 2026-06-13
**Status:** approved (B2 intervention, follows the 2026-06-09 usability spec's
measure-first commitment)
**Driver:** `ohara index` on Apple Silicon is embed-bound — the questdb
baseline (5,978 commits / 121,243 hunks) spent 22,943s of 26,069s total CPU
(87%) in the embed stage, 56.5 min wall on an M4 Pro.

## Investigation findings (2026-06-12/13, M4 Pro, macOS 26.4)

Full matrix in `tests/perf/coreml_matrix.rs`; raw numbers in
`docs/perf/v0.11-coreml-fixed-shape.md`. Summary:

1. **Today's CoreML wiring never worked for BGE.** `--embed-provider coreml`
   passes ort's default EP config — legacy NeuralNetwork format, where
   `LayerNormalization`/`Gelu` don't exist. On macOS 26 it OOM-SIGKILLs
   during CoreML compile. The README's "3-5x on embed" claim was never
   realized through this path.
2. **The quantized default can't be accelerated.** The CoreML EP supports no
   quantized ops (no QLinearMatMul/MatMulInteger/QDQ), so
   `bge-small-en-v1.5-q` INT8 matmuls always stay on CPU.
3. **MLProgram + dynamic shapes is broken too.** onnxruntime 1.24.2's
   ONNX→CoreML conversion emits unbounded tensor dimensions; the ANE rejects
   every layer (`E5RT: ... has unbounded dimension which is not supported`),
   and the CoreML-CPU fallback either segfaults in `libBNNS`
   `batch_matmul` (dynamic shapes; onnxruntime#21227 class) or explodes to
   8-12GB and gets jetsam-killed (batch ≥ 32).
4. **The plan-7 "CoreML leak" was shape-specialization churn.** With
   pad-to-longest tokenization every batch has a distinct (batch, seq) shape;
   CoreML compiles and retains a specialization per shape. Fixed shapes →
   one specialization → footprint flat at 2.08GB over 500 sustained batches
   (~2KB/batch noise vs the historical 4.3MB/batch).
5. **The fix:** bake static dims into the model (the official
   `make_dynamic_shape_fixed` guidance): `batch_size→32`,
   `sequence_length→512` on graph inputs+outputs, FP32 weights, CoreML
   MLProgram format, `ComputeUnits::All`, and keep every runtime tensor
   exactly (32, 512). Result: **118.5 rows/s vs 38.9 (today's INT8-CPU) =
   3.05×**, parity vs fp32-cpu min cosine 1.0000, no leak. ANE-only merely
   ties CPU — the win needs GPU+ANE (`All`).

Projected: questdb index ~56 min → ~20 min (embed is 87% of work).

## Design

### ohara-embed: `onnx_dims` (no new deps)

In-memory protobuf rewriter that replaces named `dim_param`s with fixed
`dim_value`s on `ModelProto.graph.{input,output}` (fields 7 → 11/12 →
ValueInfoProto.type(2).tensor_type(1).shape(2).dim(1), Dimension
dim_value=1/dim_param=2). Generic copy-through for every other field;
recursion re-emits enclosing lengths. Operates on bytes
(`fix_graph_io_dims(&[u8], &[(&str, u64)]) -> Result<Vec<u8>>`); the 127MB
initializer payload is copied verbatim. Mirrors what
`python -m onnxruntime.tools.make_dynamic_shape_fixed` produced for the
validated spike models (inputs AND outputs changed; `value_info` is empty in
this export).

### ohara-embed: `CoreMlFixedProvider`

`EmbeddingProvider` for indexing on Apple Silicon, gated behind the existing
`coreml` feature + `target_os = "macos"`:

1. Ensure the fp32 snapshot exists in the fastembed cache (construct + drop a
   CPU `TextEmbedding` for `BGESmallENV15` if missing — reuses fastembed's
   download machinery, no new HTTP dep).
2. Read `model.onnx` + the four tokenizer files from the snapshot dir; patch
   dims to (32, 512) in memory.
3. `UserDefinedEmbeddingModel` (CLS pooling) +
   `InitOptionsUserDefined { eps: CoreML MLProgram/All, max_length: 512 }`.
4. Override tokenizer padding to `PaddingStrategy::Fixed(512)` via the public
   `tokenizer` field (fastembed hardcodes `BatchLongest`, which would produce
   shape mismatches on short batches). Pad token `[PAD]`/id 0 (BGE constants).
   Requires a direct `tokenizers = "=0.22.2"` workspace dep (fastembed 5.13's
   pin) for type compatibility.
5. `embed_batch` wrapper: split into 32-row chunks, pad the tail with empty
   strings, truncate the output back. Constant (32, 512) tensors by
   construction.
6. `model_id() = "bge-small-en-v1.5"` (the true fp32 identity), `dim = 384`.

The reranker never uses CoreML (query-side, daemon-resident, and the dynamic
path is the broken one). The old dynamic
`FastEmbedProvider::with_provider(CoreMl)` remains only for the perf
harnesses.

### ohara-core: embedding-model equivalence

`bge-small-en-v1.5` and `bge-small-en-v1.5-q` form an equivalence class in
`CompatibilityStatus::assess` (measured cross-model parity: min cosine
1.0000 over full-length probes). Consequences:

- Existing INT8-built indexes keep working; incremental CoreML passes append
  fp32 vectors alongside INT8 ones by design. No forced rebuild.
- Query embeds (daemon/MCP/CLI) continue to use the INT8-CPU embedder
  regardless of which provider indexed the repo.
- A per-model query-embedder registry is explicitly deferred; if a future
  embedder is NOT vector-equivalent, it must get a new model id outside the
  class.

### ohara-cli

- `--embed-provider coreml` → `CoreMlFixedProvider` (indexing only). Without
  the `coreml` build feature it keeps returning the actionable
  rebuild-with-feature error.
- `--embed-provider auto` now resolves to CPU on macOS (no more silent
  CoreML pick + ≥1000-commit downgrade; `resolve_with_downgrade` and
  `LONG_PASS_THRESHOLD` are deleted). CoreML is opt-in this release; auto may
  flip after bake time.
- First-use message: notes the one-time fp32 download (~130MB) and the
  ~30s CoreML compile per index run.

### Out of scope (deferred)

- Flipping `auto` to CoreML on Apple Silicon (needs bake time + more
  machines; M1-class GPUs need re-measurement — ANE-only ties CPU).
- ort `with_model_cache_dir` compile caching (27s once per `ohara index`
  run is acceptable; revisit if it lands in the daemon).
- Multi-session CPU embed pool (~1.3× measured, platform-agnostic) — separate
  follow-up.
- Per-model query-embedder registry in the engine.

## Test plan

- `onnx_dims`: unit tests over hand-built minimal ModelProto bytes
  (roundtrip, dim replacement on inputs+outputs, unknown fields preserved,
  unmatched dim_params untouched, malformed input errors).
- `CoreMlFixedProvider`: hardware-bound `#[ignore]` integration test
  (embed 40 rows incl. a short-batch tail → 40 vectors, 384d, parity vs
  fp32-cpu ≥ 0.999); compile-time gate tests for the non-coreml build error.
- Equivalence: unit tests in ohara-core compat (q↔fp32 compatible in both
  directions, unknown ids still NeedsRebuild).
- CLI: provider-resolution unit tests updated (auto→cpu on macOS).
- Operator validation: `ohara index --embed-provider coreml` on questdb,
  compare wall time + `ohara query` sanity on the resulting index.
