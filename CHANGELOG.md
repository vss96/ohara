# Changelog

All notable changes to ohara are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased]

## [0.8.1] - 2026-05-06

### Changed

- **`ohara plan` is now print-only by default.** The previous behavior
  auto-wrote `.oharaignore` from the commit-share hotmap, which on
  repos where the most-touched top-level directory is the engine
  itself (e.g. QuestDB's `core/`) silently excluded the product from
  the index. The default path now prints the hotmap and suggested
  patterns and exits; pass `--write` to apply the suggestions to
  `.oharaignore`. `--replace` now requires `--write`. The `--yes` and
  `--no-write` flags and the interactive confirmation prompt are
  removed
  ([be70f9b](https://github.com/vss96/ohara/commit/be70f9b)) ([#38](https://github.com/vss96/ohara/pull/38)).

## [0.8.0] - 2026-05-06

### Added

- **`ohara plan` + `.oharaignore`** (plan-26 / Spec A). New
  `ohara plan [path]` subcommand surveys a repo's commit-share hotmap
  via a paths-only libgit2 walk, suggests high-share top-level
  directories outside a small documentation allowlist, and writes a
  marker-fenced `.oharaignore` at the repo root. The indexer consults
  a layered filter (built-in defaults + `.gitattributes` `linguist-*`
  + user `.oharaignore`); commits whose changed paths are 100% ignored
  are dropped entirely while their watermark advances. Mixed commits
  keep their non-ignored hunks
  ([ba26fb6](https://github.com/vss96/ohara/commit/ba26fb6)) ([#32](https://github.com/vss96/ohara/pull/32)).

- **`ohara index --embed-cache off|semantic|diff`** (plan-27 / Spec B).
  Content-addressable chunk-level embed cache so identical chunk
  content costs one embed call instead of one per occurrence. Three
  modes: `off` (default; today's behavior), `semantic` (cache keyed by
  `sha256(commit_msg + diff_text)`; conservative), `diff` (cache keyed
  by `sha256(diff_text)` and embedder input drops the commit message;
  high hit-rate on vendor refreshes / mass renames). Mode is part of
  the index identity — switching between `Diff` and `non-Diff` triggers
  the existing `--rebuild` flow. `ohara status` prints
  `embed_cache: <mode> (<rows> cached / <KB>)` when populated
  ([ae4f69c](https://github.com/vss96/ohara/commit/ae4f69c)) ([#33](https://github.com/vss96/ohara/pull/33)).

- **`ohara index --workers <N>`** (plan-28 / Spec D). Actor-style
  commit pipeline: walker task + N worker tasks + bounded
  `tokio::sync::mpsc` channel. Each worker owns a commit end-to-end
  (hunk_chunk → attribute → embed-with-plan-27-cache → persist).
  Default `--workers num_cpus::get()`; `--workers 1` reproduces today's
  serial path. Each commit gets a deterministic ULID derived from
  `(commit_time, commit_sha)` (V6 migration adds `commit.ulid` column +
  index); persistence is order-free; ULID-keyed reads recover
  chronological order. `ohara status` derives `last_indexed_commit`
  from `MAX(ulid)`. Per-commit failure isolation: bad commits log warn
  and the run continues
  ([288db20](https://github.com/vss96/ohara/commit/288db20)) ([#34](https://github.com/vss96/ohara/pull/34)).

### Fixed

- `Coordinator::run_from_attributed` now honors `embed_mode` and
  `cache_storage` instead of always running with `EmbedMode::Off` —
  resume-from-checkpoint paths can correctly use the chunk cache
  ([551f204](https://github.com/vss96/ohara/commit/551f204)) ([#35](https://github.com/vss96/ohara/pull/35)).
- `embed_cache_get_many` chunks lookups at 500 hashes per query to
  stay under SQLite's `SQLITE_MAX_VARIABLE_NUMBER` (default 999), so
  large commits with thousands of unique chunk hashes succeed
  ([551f204](https://github.com/vss96/ohara/commit/551f204)) ([#35](https://github.com/vss96/ohara/pull/35)).

## [0.7.7] - 2026-05-05

### Fixed

- **`ohara index` now shows progress** during the indexer dead window
  (issue [#29](https://github.com/vss96/ohara/issues/29)). The plan-19
  coordinator refactor had silently lost the per-commit progress
  callbacks; the `ProgressSink` is now threaded into `Coordinator` and
  drives `pre_walk` → `start` → per-commit `commit_done` from inside
  the loop. New `ProgressSink::pre_walk(msg)` renders a length-less
  spinner during the embedder lazy-load (~15-25 s on first run) and
  the libgit2 revwalk so the bar isn't a full ~30 s of dead air before
  it appears. `PROGRESS_INTERVAL` dropped from 100 to 25 so
  `RUST_LOG=info` users without a TTY-rendered bar still see motion
  every few seconds. Phase markers (`loading embedder` /
  `embedder loaded` / `walking commit history` /
  `commit walk complete`) print at INFO regardless of indicatif state
  ([3a1a2d0](https://github.com/vss96/ohara/commit/3a1a2d0)) ([#30](https://github.com/vss96/ohara/pull/30))
- **`ohara status` no longer dumps refinery migration logs** on every
  invocation (issue [#28](https://github.com/vss96/ohara/issues/28)).
  Default `EnvFilter` adds `refinery_core=warn` so the
  `"applying migration"` / `"no migrations to apply"` /
  `"preparing to apply N migrations: Map {…}"` chatter — including
  the full SQL of every migration as a Debug `Map` — is silenced.
  Real failures still surface at WARN/ERROR; override with
  `RUST_LOG=info,refinery_core=info ohara …` if needed
  ([3a1a2d0](https://github.com/vss96/ohara/commit/3a1a2d0)) ([#30](https://github.com/vss96/ohara/pull/30))

### Changed

- Janitor pass: hygiene + small interface tidies. One real
  `.unwrap()` in `crates/ohara-mcp/src/tools/find_pattern.rs`
  promoted to `expect("invariant: …")`; six broken intra-doc links
  fixed; plan-21 doc marked merged; mcp `invalid_params` envelope
  paths now have +10 unit tests; `ohara-storage` / `ohara-parse` /
  `ohara-git` got expanded crate-level `//!` docs;
  `hydrate_blame_results` 4-arg surface grouped into a
  `HydrateInputs` struct; `RuntimeIndexMetadata::current` collapsed
  into the single `runtime_metadata_from` constructor
  ([79c9d40](https://github.com/vss96/ohara/commit/79c9d40)) ([#26](https://github.com/vss96/ohara/pull/26))

## [0.7.6] - 2026-05-05

### Added

- TypeScript and JavaScript language support (plans 17 + 18) — tree-sitter
  parsers for `.ts` / `.tsx` / `.js` / `.jsx` / `.mjs` / `.cjs`, with the
  AST sibling-merge chunker tuned for both ([bace206](https://github.com/vss96/ohara/commit/bace206)) ([#19](https://github.com/vss96/ohara/pull/19))

### Changed

- **Indexer 5-stage pipeline (plan-19):** the indexer is now decomposed
  into discrete `walk → chunk → embed → attribute → write` stages with
  resume-agnostic semantics — a crashed pass replays cleanly without
  duplicating work ([e2ebfd8](https://github.com/vss96/ohara/commit/e2ebfd8)) ([#17](https://github.com/vss96/ohara/pull/17))
- **Retriever lanes + `ScoreRefiner` trait (plan-20):** the retriever
  pipeline split into composable lanes (vector KNN, BM25 hunk text, BM25
  symbol name) feeding a chain of `ScoreRefiner` impls (RRF, cross-encoder
  rerank, recency) ([a4e9ccf](https://github.com/vss96/ohara/commit/a4e9ccf)) ([#15](https://github.com/vss96/ohara/pull/15))
- **Explain hydrator + `ContentHash` + BlameCache wiring (plan-21):**
  `explain_change` blame computation and result hydration are now
  independently testable; daemon-warm calls skip `Blamer::blame_range`
  when the file's HEAD content hasn't changed ([8127ac0](https://github.com/vss96/ohara/commit/8127ac0)) ([#16](https://github.com/vss96/ohara/pull/16))
- **Ranking improvements re-ported onto the new retriever (plans 22–25):**
  recency formula fix (multiplicative against bounded sigmoid rerank),
  rerank pool sized to the plan-23 baseline (k=20), per-lane gate with
  batched symbol resolution, contextual BM25 lane ([4b9d3db](https://github.com/vss96/ohara/commit/4b9d3db)) ([#25](https://github.com/vss96/ohara/pull/25))
- Post-plan-16 cleanup: `compose_hint` consolidation, IPC stub refresh,
  `spawn_daemon` ergonomics, `RankingWeights` exposure ([5830e9c](https://github.com/vss96/ohara/commit/5830e9c)) ([#13](https://github.com/vss96/ohara/pull/13))
- CI: switch to `Swatinem/rust-cache` + `cargo-nextest`, drop the
  redundant `cargo build` step ([8bf15ee](https://github.com/vss96/ohara/commit/8bf15ee)) ([#18](https://github.com/vss96/ohara/pull/18))

### Fixed

- Python: extract classes that have no methods ([e5355ef](https://github.com/vss96/ohara/commit/e5355ef)) ([#24](https://github.com/vss96/ohara/pull/24))
- JavaScript / TypeScript: extract classes that have no methods ([e4c324a](https://github.com/vss96/ohara/commit/e4c324a)) ([#20](https://github.com/vss96/ohara/pull/20))

### Documentation

- Plan-18 tree-sitter modernization ([e3cd34b](https://github.com/vss96/ohara/commit/e3cd34b)) ([#12](https://github.com/vss96/ohara/pull/12))

## [0.7.5] - 2026-05-04

### Added

- Claude Code plugin scaffold ([dafbcfb](https://github.com/vss96/ohara/commit/dafbcfbd3b01275e20e84977799e9c77aa1a7f1c)) ([#6](https://github.com/vss96/ohara/pull/6))
- `ohara serve` daemon (multi-repo) + RetrievalEngine extraction ([21bc1af](https://github.com/vss96/ohara/commit/21bc1af6c251c45cd6aa932801e9315f799990a6)) ([#8](https://github.com/vss96/ohara/pull/8))

### Documentation

- Claude Code plugin install page ([e4df5f7](https://github.com/vss96/ohara/commit/e4df5f7b8b035e63aaa36eb3a55f0bb0076d3191)) ([#7](https://github.com/vss96/ohara/pull/7))
- Plan-17 TypeScript + JavaScript language support ([6bbc1a5](https://github.com/vss96/ohara/commit/6bbc1a5728ddebddb7d13ae126b4b3886bf0a8e6)) ([#9](https://github.com/vss96/ohara/pull/9))

## [0.7.4] - 2026-05-04

### Fixed

- Skip gitlink entries in `file_at_commit` ([d8c8247](https://github.com/vss96/ohara/commit/d8c82478da25013921b41a296e6c3d98e9773f11))

## [0.7.3] - 2026-05-04

### Changed

- Memory-efficient indexing (Plan-15) ([fc06c9d](https://github.com/vss96/ohara/commit/fc06c9d7e95f0893b68d157c8bc785b8ef6f0fe1)) ([#5](https://github.com/vss96/ohara/pull/5))

## [0.7.2] - 2026-05-03

### Changed

- Phase tracing + per-method storage metrics + harness binaries (Plan-14) ([18b2bea](https://github.com/vss96/ohara/commit/18b2bea7c648df77ef70d18c70aa2ba49aae79d0)) ([#3](https://github.com/vss96/ohara/pull/3))

## [0.7.1] - 2026-05-03

### Fixed

- Scope workflows write permission to host job ([316cac2](https://github.com/vss96/ohara/commit/316cac2a0ac1dec41b823649853cfdad3277b590))
- Restore release workflow parsing; make RELEASE_TOKEN optional for gh release ([92905b5](https://github.com/vss96/ohara/commit/92905b5585f06e8839423ad089f5fc47f0d303b7))

## [0.7.0] - 2026-05-03

### Added

- Context-engine retrieval-quality eval harness + golden cases (Plan-10) ([51f44d4](https://github.com/vss96/ohara/commit/51f44d481f62073ca7f1d270111c4dbdcb1cbe87))
- Deterministic context-engine eval fixture builder ([636bf8e](https://github.com/vss96/ohara/commit/636bf8e9d36d7eb0176024f417a7a3081aa2f702))
- V3 `index_metadata` table for compatibility tracking (Plan-13) ([6df910a](https://github.com/vss96/ohara/commit/6df910a6b0b70a285cace1300c921a2923e4a178))
- Index compatibility model (`RuntimeIndexMetadata` + status) ([de637de](https://github.com/vss96/ohara/commit/de637de0a4c96fafe6816a9b43d1e0a8fd8f7c8d))
- Compatibility verdict in MCP `_meta` + early fail on `NeedsRebuild` ([a4fe8f9](https://github.com/vss96/ohara/commit/a4fe8f9f25f11140a2d9b1d0ea0c75a4fd3f5d0c))
- Destructive `ohara index --rebuild` flag ([cefbc1a](https://github.com/vss96/ohara/commit/cefbc1a88d9daced6eb6e32325b40f96dd05f3d2))
- V4 migration: `hunk.semantic_text` + historical symbol attribution (Plan-11) ([4ebf07d](https://github.com/vss96/ohara/commit/4ebf07d16bd713ca46409881c532171edb54d5af))
- `HunkSymbol` + `AttributionKind`; semantic-text hunk builder wired into indexer ([26482e1](https://github.com/vss96/ohara/commit/26482e1b6fe2dfe308e3b018375180bf3ce97ac1))
- BM25 retrieval lane over `semantic_text` ([150b29d](https://github.com/vss96/ohara/commit/150b29df2f78b91c6645f7349c5e3e43d11f90ad))
- Per-hunk symbol attribution persisted + queried ([899567b](https://github.com/vss96/ohara/commit/899567ba11ca856bae48c8d1a75f2e1fe6f3bf51))
- Historical-symbol lane with HEAD-symbol fallback in retriever ([47db4f6](https://github.com/vss96/ohara/commit/47db4f68a95c7cdc5641ffb039676d4c0c196605))
- `PatternHit.related_head_symbols` populated from `hunk_symbol` rows ([0d51487](https://github.com/vss96/ohara/commit/0d51487b4c82f5a6658b6daf7ee5ff3d6ae9858c))
- `QueryIntent` + `RetrievalProfile` types + deterministic query parser (Plan-12) ([67c78a5](https://github.com/vss96/ohara/commit/67c78a5aecc557e767b4660fbe7f4ad773bc849f))
- Query profiles threaded through `find_pattern` ([a005afe](https://github.com/vss96/ohara/commit/a005afe0c316b0c00370ae5ef5347b690609a740))
- FTS5 query sanitizer + plan-12 profile eval cases ([266ccee](https://github.com/vss96/ohara/commit/266ccee32dcc10d4cc0e3e768993e29409ddc797))
- `Storage::get_neighboring_file_commits` ([25bb4ef](https://github.com/vss96/ohara/commit/25bb4ef4230c443f545ae5b5bf83b0870419c25d))
- Contextual related-commits enrichment in `ExplainMeta` ([3a94698](https://github.com/vss96/ohara/commit/3a94698e90e16b7ec21f49571327543459d041a8))

### Fixed

- Grant workflows write permission for dist releases ([05dd909](https://github.com/vss96/ohara/commit/05dd90918d7dc70761975699955894255aa05cd8))
- Qualify anyhow macros for clippy `--all-features` on macOS ([dae78ee](https://github.com/vss96/ohara/commit/dae78eea487d26e2e0b2b9e9bb69de233652f625))

## [0.6.3] - 2026-05-03

### Added

- `Storage::commit_exists` for resume skip-check ([1018bd2](https://github.com/vss96/ohara/commit/1018bd2c5b0d4236c285277306e85689b92e6be7))
- Skip already-indexed commits on resume to avoid duplicate embedding work ([cd371ef](https://github.com/vss96/ohara/commit/cd371ef5d0dd83060544a4cffd54dc1506560a64))

## [0.6.2] - 2026-05-02

### Added

- CoreML wired into the released `aarch64-apple-darwin` binary ([679714a](https://github.com/vss96/ohara/commit/679714aa2f38ab336a21b8b21159215deb71b858))

### Fixed

- Follow-up tightening of v0.6.1 CoreML auto-downgrade ([70f542f](https://github.com/vss96/ohara/commit/70f542f066d491a7af542829f4aac0103b772901))

## [0.6.1] - 2026-05-02

### Added

- Auto-downgrade CoreML to CPU on long index passes to mitigate memory leak ([1df2001](https://github.com/vss96/ohara/commit/1df20013c85a9ba3220127f031d37e691c2de980))

## [0.6.0] - 2026-05-02

### Added

- Per-commit progress logging + structured indexed event ([f6c7a2c](https://github.com/vss96/ohara/commit/f6c7a2cd1a24c8244b8699017bea45e1239bca59))
- `--profile` flag emits `PhaseTimings` JSON to stdout ([a97098b](https://github.com/vss96/ohara/commit/a97098be0152873b99944c0b6779331a1b942f6e))
- `EmbedProvider` enum + `--embed-provider {auto,cpu,coreml,cuda}` CLI flag ([c8a5f5d](https://github.com/vss96/ohara/commit/c8a5f5db3b338de9d7c4bbc8bebaf62dec780461))
- ORT `coreml`/`cuda` features wired through `ohara-embed` and `ohara-mcp` ([9ddb046](https://github.com/vss96/ohara/commit/9ddb046e056fae585a11a524257205a24528a422))
- `--resources` flag for overriding CPU/memory limits in `ohara index` ([2767f03](https://github.com/vss96/ohara/commit/2767f032d86db98f92c5cb627508ced950d0272d))
- `tracing-indicatif` TUI progress bar for `ohara index` ([41bfa7d](https://github.com/vss96/ohara/commit/41bfa7d17befea4fce1d9d7b2170d9ea3cb3db39))
- Git SHA injected into `ohara --version` output ([ace1b91](https://github.com/vss96/ohara/commit/ace1b914bc2ed6b3ce703497318b97a6d37cbd79))
- Informational weekly perf workflow + context-engine eval CI job ([cfc8390](https://github.com/vss96/ohara/commit/cfc83908b76b292c6e8ea42b7f54aaffa8ba1403))
- mdBook documentation site + GitHub Pages workflow ([d075b05](https://github.com/vss96/ohara/commit/d075b05b4c76ced615d6997d100922bd9e4e507d))
- Resources lookup table + `ResourcePlan` ([e359778](https://github.com/vss96/ohara/commit/e3597788c4d0c3be98566537f34fb72480439292))

### Fixed

- Resume crash: `vec_commit`/`fts_commit` DELETE-then-INSERT in `commit::put` ([2a7fea6](https://github.com/vss96/ohara/commit/2a7fea644d08485296d29bd20a7b2d507d1ad7c8))

### Changed

- `CommitMeta.sha` renamed to `commit_sha` for consistency ([36ee51c](https://github.com/vss96/ohara/commit/36ee51cc261f4fad4961f94202f84b34867fc473))
- `ohara-storage` reorganized into `tables/` and `codec/` submodules ([b70968e](https://github.com/vss96/ohara/commit/b70968e940b228cca8f22886a880a6bf0c00df68))
- `ohara-parse` language extractors reorganized under `languages/` ([9ec8e55](https://github.com/vss96/ohara/commit/9ec8e55355061d2d207a61730e2f26146b6ce3db))
- `diff_text` helpers deduplicated into `ohara-core::diff_text` ([856bed1](https://github.com/vss96/ohara/commit/856bed197331e75a7ec7d865d96187dc4e6d0445))
- Change-kind/file-path codec centralized into `ohara-storage::row_codec` ([707dc63](https://github.com/vss96/ohara/commit/707dc63048edc3d204b1f8fab94d22b01befe25b))

## [0.5.1] - 2026-05-01

### Added

- Per-100-commits progress logging + structured indexed event ([855dc9f](https://github.com/vss96/ohara/commit/855dc9f2deba4e37c4c5f94e047f4971150d2336))
- Progress bar, `--commit-batch`/`--threads` flags, abort-resume safety to `ohara index` ([526af35](https://github.com/vss96/ohara/commit/526af3583d8b83ad93a79bf5364e1b4baf938e24))
- `ohara update` self-upgrade subcommand + cargo-dist install-updater ([08e4e27](https://github.com/vss96/ohara/commit/08e4e2778eca45f15583068fadec7cb3eed7f7fc))

## [0.5.0] - 2026-05-01

### Added

- `Storage::get_commit` + `get_hunks_for_file_in_commit` trait + implementation ([852fc6f](https://github.com/vss96/ohara/commit/852fc6f195c38ce2686675cd91eb70206d413b6e))
- `BlameSource` trait + `ExplainQuery`/`Hit`/`Meta` types + exact-provenance model ([f9b85b6](https://github.com/vss96/ohara/commit/f9b85b63017e76c4ded248d0e4b2fec0aaff935d))
- `Blamer::blame_range` over `Arc<Mutex<Repository>>` with `spawn_blocking` ([1cc80f6](https://github.com/vss96/ohara/commit/1cc80f650bb145198cb921508b882376561e2a4b))
- `explain_change` orchestrator: blame → commits → hunks ([ed3b753](https://github.com/vss96/ohara/commit/ed3b7530c55cb08839ef18fb5d1b4048bd88d594))
- `explain_change` MCP tool wired onto `OharaService` + `Blamer` ([5fed7b5](https://github.com/vss96/ohara/commit/5fed7b500300893cf820d8488fb257a9ee97ebf8))
- `ohara explain` CLI subcommand ([3243a0a](https://github.com/vss96/ohara/commit/3243a0a5f3ad40baf3bd6eaf0934ac0ae84d18e2))

## [0.4.0] - 2026-05-01

### Added

- Java language support: class, interface, method, constructor, record, enum, annotation types ([06736ec](https://github.com/vss96/ohara/commit/06736ec594af8b8d397477929c7d24c7a41b1dd4))
- `.java` files routed in `extract_for_path` ([ecaa45f](https://github.com/vss96/ohara/commit/ecaa45fda0ebc0a5687edb1e1f79494023817aad))
- Kotlin language support: class, object, companion, function/method discrimination ([346245b](https://github.com/vss96/ohara/commit/346245b7adf393c35ad44561eb7b993b82859b64))
- `.kt` and `.kts` files routed in `extract_for_path` ([c8326d2](https://github.com/vss96/ohara/commit/c8326d2dabbe4a3bcd2e5ab77d45f81004ed41b0))
- Spring-flavored integration fixture for end-to-end Java + Kotlin coverage ([715cc5d](https://github.com/vss96/ohara/commit/715cc5d6be0653ec993a00c5ce4ee54f7dd4eb9f))

## [0.3.0] - 2026-05-01

### Added

- V2 migration: FTS5 BM25 retrieval lanes + `sibling_names` column ([5375ac9](https://github.com/vss96/ohara/commit/5375ac9ee4b70a0e1eaf54b253da5bfd289d4647))
- Storage trait surface for BM25 lanes + `HunkId` join key ([7995665](https://github.com/vss96/ohara/commit/7995665783b829d839821da72f688367cceae7b2))
- BM25 lane persistence + `put_head_symbols` ([2ac78b7](https://github.com/vss96/ohara/commit/2ac78b7b754d664fc647f5640e29e5b2f1863ae6))
- `RerankProvider` trait + `FastEmbedReranker` with `bge-reranker-base` ([b48b7f1](https://github.com/vss96/ohara/commit/b48b7f1beaaaab400fe926d6deaa5d6b012de9e6))
- `Symbol::sibling_names` field for AST sibling-merge chunker context ([85a189d](https://github.com/vss96/ohara/commit/85a189d5f65097f850bdb0bb3efdcd8f23b198ee))
- AST sibling-merge chunker (`chunk_symbols`) wired into `extract_for_path` ([3dba622](https://github.com/vss96/ohara/commit/3dba622dcad9b336ead0ef2d007c61723029f8b1))
- Reciprocal Rank Fusion implementation (1-based ranks, first-seen tie-break) ([d0f3c42](https://github.com/vss96/ohara/commit/d0f3c42ad82a402ee152acc7f210b61f528add83))
- v0.3 retrieval pipeline: three lanes (vector, BM25 hunk, BM25 symbol) → RRF → optional rerank → recency ([eed7b37](https://github.com/vss96/ohara/commit/eed7b37f80ebc75fd7cbb9f991f70dac728fd23e))
- `--force` flag for clean HEAD symbol rebuild ([0920ffc](https://github.com/vss96/ohara/commit/0920ffcf4438e60c51c80b788197cf18ce673197))
- `--no-rerank` flag plumbed through MCP tool and `PatternQuery` ([9748f7e](https://github.com/vss96/ohara/commit/9748f7e402aa05d99f070556bf85836791c9c92c))

## [0.2.0] - 2026-05-01

### Added

- `ohara-core::paths` helpers deduplicated from inline copies ([d5b5f8e](https://github.com/vss96/ohara/commit/d5b5f8e13303a29aafc4f55e98eb48fde113c2c1))
- `compute_index_status` + callers rewired ([930147f](https://github.com/vss96/ohara/commit/930147f4eff29aff6a8760c7f99a7d1a36aa504b))
- `ohara index --incremental` HEAD-equals-watermark fast path ([c3db676](https://github.com/vss96/ohara/commit/c3db676764bbbacbbb1c1a68d282e85977702331))
- `ohara init`: marker-aware post-commit hook writer ([1b141db](https://github.com/vss96/ohara/commit/1b141dba728ddb9d4945ffc068932e8233d4d032))
- `ohara init --write-claude-md`: marker-aware CLAUDE.md stanza writer ([3d10c02](https://github.com/vss96/ohara/commit/3d10c0287fb6d72075df5ff1777f3c403b4e10f9))
- `OharaError::Config` variant for config-time errors ([0c618ed](https://github.com/vss96/ohara/commit/0c618ed4c7cfae740869d362caa08e256da3e48e))

### Fixed

- Pin rmcp to `=0.1.5` for stable API surface ([9935c7b](https://github.com/vss96/ohara/commit/9935c7bf369d6a7ecce5366d38ef43186b762599))
- Drop dead `OharaServer::embedder` field ([0acf38a](https://github.com/vss96/ohara/commit/0acf38a97c5c2d9f35bec7f37009088647898512))

[Unreleased]: https://github.com/vss96/ohara/compare/v0.7.5...HEAD
[0.7.5]: https://github.com/vss96/ohara/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/vss96/ohara/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/vss96/ohara/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/vss96/ohara/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/vss96/ohara/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/vss96/ohara/compare/v0.6.3...v0.7.0
[0.6.3]: https://github.com/vss96/ohara/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/vss96/ohara/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/vss96/ohara/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/vss96/ohara/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/vss96/ohara/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/vss96/ohara/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/vss96/ohara/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/vss96/ohara/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/vss96/ohara/releases/tag/v0.2.0
