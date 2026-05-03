# Changelog

User-facing release notes. The full commit log lives on
[GitHub](https://github.com/vss96/ohara/commits/main); this page is
the highlights.

## v0.7.1 — Release CI token scope

No user-facing binary changes. Fixes GitHub Actions `GITHUB_TOKEN`
permissions so `cargo-dist` can create a GitHub Release when the tagged
commit touches `.github/workflows/**` (`workflows: write` alongside
`contents: write`; see [cli/cli#9514](https://github.com/cli/cli/issues/9514)).

## v0.7.0 — Retrieval-quality eval, semantic-text + per-hunk symbol attribution, query intent, explain enrichment, index compatibility

Four plans land together: a regression-tripwire eval harness
(plan 10), historical per-hunk symbol attribution + a normalized
semantic-text representation (plan 11), rule-based query
understanding + contextual `explain_change` enrichment (plan 12),
and an index-metadata + rebuild-safety layer that keeps old
indexes from silently producing wrong results (plan 13).

### Retrieval

- **Per-hunk symbol attribution.** A new `hunk_symbol` table
  records which symbols inside a file each hunk actually touched.
  `bm25_hunks_by_historical_symbol` is the new primary symbol
  retrieval lane; the v0.3 file-level lane is kept as fallback for
  pre-v0.7 indexes. Two confidence kinds: `ExactSpan` (a parsed
  symbol's line span intersects the hunk's `@@` post-image range,
  derived from the post-image source via the new `extract_atomic_symbols`
  parser entry-point) and `HunkHeader` (git's `@@` line includes
  an enclosing function/class). Solves the file-level lane's
  wrong-hunk problem: a query for `retry_policy` no longer returns
  every hunk in a file containing it, only the hunk that actually
  touched `retry_policy`. `PatternHit.related_head_symbols` now
  carries the touched-symbol names instead of staying empty.
- **Semantic hunk text.** A new `hunk.semantic_text` column stores
  a normalized representation (commit / file / language / symbols
  / change / added_lines sections) that the embedder + a new
  `bm25_hunks_by_semantic_text` lane (`fts_hunk_semantic`) see in
  place of raw `diff_text`. The raw diff stays put for display
  and provenance. Backfill: existing hunks get
  `semantic_text = diff_text` so the new lane returns useful
  results immediately on a v0.6 index, and a fresh index pass
  populates real semantic text.
- **Query understanding.** A new rule-based parser
  (`ohara_core::query_understanding::parse_query`) classifies
  free-form queries into one of `ImplementationPattern`,
  `BugFixPrecedent`, `ApiUsage`, `Configuration`, or `Unknown`,
  and extracts explicit filters (language hints, path tokens,
  quoted symbol names, simple `last N days/weeks/months`
  timeframes). `find_pattern_with_profile` returns the picked
  `RetrievalProfile` alongside hits so MCP responses surface
  `_meta.query_profile` (name + explanation). Profile behaviour
  is conservative: bug-fix bumps recency 1.5x, API-usage widens
  the rerank pool, configuration disables the symbol lane,
  unknown intent uses today's defaults byte-identically.
- **FTS5 query sanitizer (bug fix).** Any user query containing
  backticks, parens, `*`, `^`, `+`, `-`, etc. previously crashed
  with `fts5: syntax error`. The BM25 lanes now strip non-word
  characters before passing the query to FTS5.

### `explain_change` enrichment

- **Two questions in one tool.** `hits[]` still carries
  blame-exact attribution (`provenance = "EXACT"`) for "which
  commits introduced these lines". A new
  `_meta.explain.related_commits[]` carries file-scope contextual
  neighbours (`provenance = "INFERRED"`) for "what nearby changes
  shaped this area". Capped at 2 commits before + 2 commits after
  each blame anchor, deduped across anchors. Backed by the new
  `Storage::get_neighboring_file_commits`. CLI defaults
  `include_related = true`; MCP defaults `false` to keep the
  response payload predictable.

### Index compatibility (plan 13)

- **`index_metadata` table.** Every successful index pass now
  records the embedder model + dimension, reranker model, AST
  chunker version, semantic-text version, schema version, and
  per-language parser versions it ran with. The runtime compares
  the recorded values against what the current binary expects
  and produces one of `Compatible`, `QueryCompatibleNeedsRefresh`,
  `NeedsRebuild`, or `Unknown`.
- **Surfaces.** `ohara status` prints a `compatibility:` line
  with an actionable next-step command (no embedder load — status
  stays cheap). MCP `_meta.compatibility` carries the structured
  verdict; `find_pattern` refuses to run on `NeedsRebuild`
  (returns a structured `invalid_params` error naming the rebuild
  command) because KNN against a stale-vector index would
  silently produce wrong results. `explain_change` continues
  under every verdict because blame doesn't depend on
  embedder/chunker/parser state.
- **`ohara index --rebuild --yes`.** Destructive recovery flag
  for `NeedsRebuild` verdicts. Refuses without `--yes`; verifies
  the DB path is under `OHARA_HOME` before deletion; removes the
  main DB plus its `-wal` / `-shm` sidecars, then runs the
  normal index pipeline against the fresh DB.

### Eval harness (plan 10)

- **`#[ignore]`'d retrieval-quality runner** under
  [`tests/perf/context_engine_eval.rs`](https://github.com/vss96/ohara/blob/main/tests/perf/context_engine_eval.rs).
  Indexes a deterministic synthetic fixture and runs every case
  in `tests/perf/fixtures/context_engine_eval/golden.jsonl`
  through the same `Retriever::find_pattern` path the CLI / MCP
  use. Emits a one-line JSON summary on stderr (cases,
  recall_at_1, recall_at_5, mrr, ndcg_lite, p50_ms, p95_ms,
  failed_ids) so PR descriptions can paste it verbatim.
  Hard contracts: `recall_at_5 == 1.0`, `mrr >= 0.80`, no
  individual query over 2 s. Profile-mismatch warnings are
  informational, not gating. CONTRIBUTING.md §14 codifies the
  PR rule: any change to retrieval ranking, chunking, hunk
  text, symbol attribution, or query parsing must run the
  harness and paste the JSON summary.
- Initial pass on the in-repo fixture: 8 cases,
  `recall_at_1=0.875`, `recall_at_5=1.0`, `mrr=0.9375`,
  `nDCG-lite=0.95`, `p95=1245ms`.

### Schema migrations

- **V3 (`index_metadata`).** Per-component compatibility tracking.
- **V4 (`hunk.semantic_text` + `hunk_symbol` + `fts_hunk_semantic`).**
  Backfill makes both new tables immediately queryable on a
  pre-v0.7 index; richer attribution and proper semantic text
  populate after a fresh `ohara index --force` (or any subsequent
  index pass).

### Upgrading

A pre-v0.7 index still works against v0.7 binaries — `ohara status`
will print `compatibility: query-compatible, refresh recommended`
because the chunker version bumped from `1` to `2`. Run
`ohara index --force` to repopulate the derived rows. Indexes
built before the v0.6.x embedder changes (different model or
dimension) report `compatibility: needs rebuild`; use
`ohara index --rebuild --yes` to drop and rebuild from scratch.

The full per-plan breakdown lives in
[`docs/superpowers/plans/`](https://github.com/vss96/ohara/tree/main/docs/superpowers/plans).

## v0.6.3 — Skip already-indexed commits on resume

Resume on a merge-heavy history no longer re-embeds commits that are
already in the index. Diagnosed on a live QuestDB resume: 273 commits
with existing `commit_record` rows were re-walked and re-embedded
because the single-SHA watermark only excludes the strict ancestor
chain, missing anything reachable via a feature-branch merge or
history rewrite. That cost ~14 minutes of CPU per resume cycle.

- **New `Storage::commit_exists` PK lookup.** Sub-millisecond per
  commit via `SELECT 1 FROM commit_record WHERE sha = ? LIMIT 1`. On
  cold-index runs the added cost is ~0.1 s across thousands of
  commits — well under measurement noise.
- **Indexer short-circuit.** The per-commit loop in `Indexer::run`
  now consults `commit_exists` before any hunk extraction or
  embedder work and `continue`s on a hit, with a `tracing::debug!`
  line for observability.
- **Watermark advances through skip stretches.** A Ctrl-C right after
  a long all-skip stretch leaves the watermark at the most recently
  walked sha, so the next resume doesn't have to re-iterate them
  either.
- **No schema migration.** The lookup hits the existing
  `commit_record.sha` primary-key index.

Design notes:
[`docs/superpowers/specs/2026-05-02-ohara-v0.6.3-resume-skip-rfc.md`](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-02-ohara-v0.6.3-resume-skip-rfc.md).

## v0.6.2 — Per-host distribution: CoreML in the Apple Silicon binary

The released binary on `aarch64-apple-darwin` now bundles the CoreML
execution provider. `ohara update` from a v0.6.1 install pulls the
new artifact in transparently — the `aarch64-apple-darwin` asset
name and `-update` shim are unchanged, so axoupdater follows
without intervention.

- **Apple Silicon users no longer need a source rebuild for CoreML.**
  The auto-downgrade shipped in v0.6.1 (`--embed-provider auto` →
  CPU on long passes, CoreML on short ones) becomes load-bearing on
  the released binary: `query` and short `--incremental` calls now
  hit CoreML by default, while 1,000+ commit cold-index passes
  still fall back to CPU to dodge the
  [`embed_batch` leak](https://github.com/vss96/ohara/blob/main/docs/perf/v0.6.1-leak-diagnosis.md).
- **Linux x86_64 / Linux aarch64 / Intel macOS artifacts unchanged.**
  cargo-dist 0.31 has no per-target features override, so we pass
  `features = ["coreml"]` to every target's `cargo build` and let
  `ohara-embed`'s target-conditional `ort` dependency strip the
  CoreML wiring on non-macOS triples. No extra ort weight on Linux
  artifacts; no link changes; no asset-name churn.
- **A `-cpu` opt-out artifact is *not* shipped.** cargo-dist 0.31
  does not support multiple builds per target, so the CoreML build
  is the only Apple Silicon artifact. Users who want pure CPU
  inference can still pass `--embed-provider cpu` (or rely on the
  long-pass auto-downgrade); they just don't get a smaller binary.
- **Internal:** `crates/ohara-embed/src/fastembed.rs` tightens the
  `EmbedProvider::CoreMl` cfg gate from `feature = "coreml"` to
  `all(feature = "coreml", target_os = "macos")`. Source builds
  (`cargo build --release`) stay CPU-only by default; the CoreML
  feature flag only flips on for the cargo-dist released binary.

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
  for long index passes** (1,000 commits or more to walk).
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
