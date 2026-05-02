# Changelog

User-facing release notes. The full commit log lives on
[GitHub](https://github.com/vss96/ohara/commits/main); this page is
the highlights.

## v0.6.1 — CoreML long-pass auto-downgrade

Workaround release for the CoreML embedder leak called out under
v0.6.0's "Known issues". Diagnosis is in
[`docs/perf/v0.6.1-leak-diagnosis.md`](https://github.com/vss96/ohara/blob/main/docs/perf/v0.6.1-leak-diagnosis.md):
the leak is heap-attributable (~4 MB / `embed_batch`, `MALLOC_LARGE`),
not an `MLModel` / ANE-side retention. Rebuild-cadence probes
mitigate by ~2× but don't bound the growth, so v0.6.1 ships a
documented workaround rather than an in-tree fix; the upstream
investigation in fastembed / ort is re-opened.

- **`--embed-provider auto` now downgrades to CPU on Apple Silicon
  for long index passes** (more than 1,000 commits to walk).
  Short-lived `query` and `index --incremental` calls keep the
  CoreML auto-pick. Threshold lives in
  [`crates/ohara-cli/src/commands/provider.rs`](https://github.com/vss96/ohara/blob/main/crates/ohara-cli/src/commands/provider.rs).
- **Explicit `--embed-provider coreml` is honoured unchanged**, with
  a one-time `tracing::warn!` on startup pointing at the diagnosis
  doc. Bypass the downgrade with this flag if you have headroom and
  want the speedup.
- **`tests/perf/coreml_leak_repro.rs`** (`#[ignore]`'d) ships as the
  regression harness. Re-run with `--features ohara-embed/coreml`
  on Apple Silicon when an upstream candidate fix lands.

### Migration

Anyone using v0.6.0 with the documented workaround
(`--embed-provider cpu`) can drop the flag — `--embed-provider auto`
now does the same thing for long passes. CUDA and CPU users see no
change.

## v0.6.0 — Throughput prep, hardware acceleration opt-in

- **`--embed-provider {auto,cpu,coreml,cuda}`** CLI flag with
  auto-detect (CoreML on Apple silicon, CUDA when
  `CUDA_VISIBLE_DEVICES` is set, else CPU).
- **`--resources {auto,conservative,aggressive}`** resource policy
  picks `--commit-batch` / `--threads` / `--embed-provider` defaults
  from host core count; explicit flags still override.
- **`--profile`** dumps per-phase wall-time JSON for benchmarking;
  feeds the v0.6 throughput baseline at `docs/perf/v0.6-baseline.md`.
- **`--no-progress`** suppresses the progress bar in CI (structured
  `tracing::info!` events still fire every 100 commits).
- **tracing-indicatif:** progress bar pinned to the bottom of the
  terminal, `tracing` log lines stream above without scrolling the
  bar away.
- **Cargo features `coreml` and `cuda`** wire ONNX execution
  providers through `ohara-embed` → `ohara-cli` / `ohara-mcp`. The
  cargo-dist release binaries stay CPU-only; build from source with
  `--features coreml` (Apple silicon) or `--features cuda` (Linux
  NVIDIA) to opt in.
- **Resume-crash fix:** `commit::put` is now DELETE-then-INSERT for
  `vec_commit` and `fts_commit`. Closes a "UNIQUE constraint failed"
  crash on resume after a kill mid-walk.
- **`ohara --version`** now reports `ohara 0.6.0 (<sha>)` so local
  builds are distinguishable from released ones.
- **Internal:** `ohara-storage/src/` split into `tables/` + `codec/`;
  `ohara-parse/src/` extractors consolidated under `languages/`. No
  public API change.

### Known issues

- **CoreML memory leak on long cold-index runs (Apple silicon).** On a
  5,000+ commit first-time `ohara index` with `--embed-provider
  coreml`, memory grows unbounded (observed 32 GB+ before macOS
  jetsam kills the process). The leak appears specific to repeated
  small-batch inference through `ort`'s CoreML provider; the CPU and
  CUDA paths are unaffected. **Documented workaround in v0.6.1**:
  `--embed-provider auto` downgrades to CPU for long passes on
  Apple Silicon — see the v0.6.1 entry above and
  [`docs/perf/v0.6.1-leak-diagnosis.md`](https://github.com/vss96/ohara/blob/main/docs/perf/v0.6.1-leak-diagnosis.md).

## v0.5.1

- **Self-update.** `ohara update` (and `--check` / `--prerelease`)
  drives [axoupdater](https://github.com/axodotdev/axoupdater) to
  install the latest release in place — same source of truth as the
  cargo-dist installer.
- **Progress bar.** Indexing now shows a live progress bar on TTY
  stderr (suppress with `--no-progress`), plus structured
  `tracing::info!` events every 100 commits for log aggregators.
- **Abort-resume hardening.** Watermark advances every 100 commits
  inside the indexer; a Ctrl-C / kill / crash mid-walk loses ≤ 100
  commits of work, never duplicates rows.

## v0.5

- **`explain_change` MCP tool.** Given a file + line range, returns
  the commits that introduced and shaped those lines, newest-first.
  Backed by `git blame`, not embeddings — every result has
  `provenance = "EXACT"`.
- **`ohara explain` CLI.** Same JSON envelope as the MCP tool, for
  debugging and `jq` piping.

## v0.4

- **Java 17 / 21 support.** Classes (incl. sealed), interfaces,
  records, enums, methods. Tree-sitter-java grammar.
- **Kotlin 1.9 / 2.0 support.** Classes (incl. sealed), data
  classes, objects + companion objects, interfaces, top-level +
  member functions. Tree-sitter-kotlin grammar.
- **Annotations preserved in `source_text`.** Spring-flavored
  markers (`@RestController`, `@Service`, `@Component`,
  `@SpringBootApplication`, …) and Kotlin annotations
  (`@Composable`, `@Serializable`) are retained verbatim, so
  embeddings and BM25 pick them up without new query syntax.

## v0.3

- **Three-lane retrieval pipeline.** `find_pattern` now dispatches
  three queries in parallel: vector KNN over `vec_hunk`, FTS5 BM25
  over hunk-text, FTS5 BM25 over symbol-name. Reciprocal Rank Fusion
  (k=60) merges them.
- **Cross-encoder rerank.** Top-50 RRF candidates re-scored by
  `bge-reranker-base` (~110 MB ONNX, CPU). Opt-out via `--no-rerank`
  / `no_rerank: true`.
- **Recency as tie-breaker only.** The v0.1 `0.7·sim + 0.2·recency
  + 0.1·msg_sim` linear formula is gone; recency is now a small
  multiplicative nudge applied after the cross-encoder.
- **AST sibling-merge chunking.** `ohara-parse` now merges sibling
  AST nodes up to a 500-token budget instead of one chunk per
  top-level symbol — better recall on small functions, fewer giant
  chunks.

## v0.2

- **`ohara init`.** Installs a managed post-commit hook so the index
  stays fresh after every commit. Idempotent (re-running is safe);
  fails-closed if `ohara` isn't on `PATH` (never blocks a commit).
  `--write-claude-md` adds an "ohara" stanza to `CLAUDE.md`.
- **`ohara index --incremental`.** Fast no-op when HEAD is already
  indexed — skips embedder boot entirely. The post-commit hook uses
  this; sub-second on no-op re-indexes.

## v0.1

- **Foundation.** Workspace of seven crates, SQLite +
  [sqlite-vec](https://github.com/asg017/sqlite-vec) + FTS5 schema,
  refinery migrations, BGE-small embeddings via
  [fastembed-rs](https://github.com/Anush008/fastembed-rs).
- **`find_pattern` MCP tool.** First version: linear-blend ranking
  over `vec_hunk` similarity + commit recency + commit-message
  similarity. Replaced by the three-lane pipeline in v0.3.
- **`ohara index`, `ohara query`, `ohara status` CLI.** Full
  index, ad-hoc query, freshness inspection.
- **Rust + Python language support.** Tree-sitter symbol extraction
  for both.
- **Distribution.** cargo-dist installer for macOS (Apple silicon +
  Intel) and Linux (`aarch64` + `x86_64`).
