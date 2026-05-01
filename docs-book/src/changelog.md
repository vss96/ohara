# Changelog

User-facing release notes. The full commit log lives on
[GitHub](https://github.com/vss96/ohara/commits/main); this page is
the highlights.

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
