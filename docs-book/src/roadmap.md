# Roadmap

## Released

| Version | Theme | Headline |
|---------|-------|----------|
| **v0.1** | Foundation | Plan 1 — workspace + SQLite/sqlite-vec/FTS5 schema + `find_pattern` MCP tool. |
| **v0.2** | Auto-freshness | Plan 2 — `ohara init` post-commit hook + `ohara index --incremental` fast path. |
| **v0.3** | Retrieval quality | Plan 3 — three-lane retrieval (vector + BM25 hunk-text + BM25 symbol-name) → RRF → cross-encoder rerank → recency tie-break. AST sibling-merge chunking. |
| **v0.4** | Java + Kotlin | Plan 4 — Java 17/21 (sealed types, records) and Kotlin 1.9/2.0 (data classes, objects). Annotations preserved in `source_text` for Spring-friendly retrieval. |
| **v0.5** | `explain_change` | Plan 5 — second MCP tool, deterministic git-blame-backed lookup of "why does THIS code look the way it does?". |
| **v0.5.1** | Polish | Progress bar, abort-resume hardening, `ohara update` self-update via axoupdater. |
| **v0.6.0** | Throughput prep | `--profile` PhaseTimings JSON, `--embed-provider` auto-detect, `--resources` policy, CoreML/CUDA feature wiring, resume-crash fix, pinned progress bar. |
| **v0.7.0** | Evals + attribution | Plan 10/11/13 — eval harness, historical symbol attribution, index metadata + rebuild safety. |
| **v0.7.2** | Perf tracing | Plan 14 — phase tracing, per-method storage metrics, perf harness binaries. |
| **v0.7.3** | Memory-efficient indexing | Plan 15 — `embed_batch` chunking + source-text cap + peak-RSS sampler. |
| **v0.7.4** | Submodule fix | Gitlink-skip in `file_at_commit` for uninitialized submodules. |
| **v0.7.5** | Daemon + multi-repo | Plan 16 — `ohara serve` daemon, `RetrievalEngine`, multi-repo support, `ohara daemon` subcommands. |

The [Changelog](./changelog.md) has a per-tag breakdown.

## In flight

### v0.7.x — TypeScript / JavaScript support (plan-17)

The next active plan adds a tree-sitter grammar for TypeScript and
JavaScript so `find_pattern` and `explain_change` work on TS/JS repos
without requiring a separate indexing step. See
`docs/superpowers/plans/2026-05-04-ohara-plan-17-typescript-javascript.md`.

## Considered for later

These come from the v0.3 spec's "Out of scope" list and the v0.6
RFC's deferred items. Not committed; revisited as evidence accrues.

- **LLM-distilled commit summaries** — replace `commit_msg +
  first_hunk` embedding with a "primary goal / key files / technical
  terms" summary against a local model. Wait until A/B against the
  v0.3 baseline shows it matters.
- **Bit-quantized vec index** — premature at our scale today;
  revisit once cold-index size matters more than retrieval latency.
- **Branch-reachability filter** — useful for users who switch
  branches often. Currently `find_pattern` returns hits across all
  reachable history.
- **Merkle file hashing** (Cursor-style) — only meaningful when ohara
  expands beyond commit history into current-state code retrieval.
- **HyDE query rewriting** — generate a hypothetical answer with a
  local LLM, embed *that*, and retrieve against the hypothetical.
  Mixed evidence in benchmarks; A/B before committing.
- **Query understanding pre-pass** — wait for evidence that
  raw-query embeddings underperform.
- **Task-specific retrieval profiles** — only meaningful once ohara
  has more than two MCP tools.
- **More languages** — Go, TypeScript, C# are the obvious next
  candidates. Mostly a tree-sitter grammar pin + a small
  node-kind → symbol-kind mapping.
- **Branch / PR-aware `explain_change`** — currently blame walks
  `HEAD`. Walking a feature branch's range against `main` is a
  natural extension.
