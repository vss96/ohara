# Workspace overview

ohara is a Cargo workspace of seven library/binary crates plus an
out-of-band perf harness. Crate boundaries follow the rule **keep the
core git-free**: the only crates that depend on `git2` or
`tree-sitter` are the adapters (`ohara-git`, `ohara-parse`); everything
else talks to them through `async_trait` ports defined in
`ohara-core`.

## Dependency direction

```
            ┌────────────┐
            │  ohara-cli │  ohara binary (commands/*)
            └─────┬──────┘
                  │ uses
   ┌──────────────┼─────────────┐
   ▼              ▼             ▼
┌──────────┐ ┌──────────┐ ┌──────────┐
│ohara-mcp │ │ohara-git │ │ohara-parse│
└────┬─────┘ └────┬─────┘ └────┬─────┘
     │            │ adapters   │
     │            ▼            ▼
     │      ┌────────────────────────┐
     ├────► │      ohara-core        │  traits + orchestration
     │      └─────────┬──────────────┘
     │                │ uses
     │                ▼
     │      ┌──────────────────┐  ┌────────────┐
     └────► │ ohara-storage    │  │ ohara-embed│
            │ (sqlite + vec +  │  │ (fastembed)│
            │  fts5)           │  │            │
            └──────────────────┘  └────────────┘
```

`ohara-mcp` is the second binary (`ohara-mcp`); it composes the same
core + storage + embed + git + parse stack the CLI uses.

## Crates

### `ohara-core`

The orchestration layer. Defines `Indexer`, `Retriever`, the
`explain_change` orchestrator, and the `async_trait` ports they talk
through (`Storage`, `EmbeddingProvider`, `RerankProvider`,
`CommitSource`, `SymbolSource`, `BlameSource`, `CommitsBehind`,
`ProgressSink`). Knows nothing about git or tree-sitter — those are
hidden behind the traits, which keeps the core unit-testable with
in-memory fakes.

### `ohara-storage`

SQLite + [`sqlite-vec`](https://github.com/asg017/sqlite-vec) +
FTS5 backend. Owns the on-disk index format, the refinery migrations
under `migrations/V*.sql`, and the implementations of every storage
trait the core declares. See [Storage schema](./storage.md).

### `ohara-embed`

Local embedding + cross-encoder reranker via
[`fastembed-rs`](https://github.com/Anush008/fastembed-rs). Wraps the
BGE-small embedding model (~80 MB, 384-dim) and `bge-reranker-base`
(~110 MB) behind the `EmbeddingProvider` and `RerankProvider` traits.
First call downloads the model; subsequent calls hit the local cache.

### `ohara-git`

`git2`-backed implementations of `CommitSource`, `BlameSource`, and
`CommitsBehind`. The only crate (besides the CLI's repo-discovery
helper) that opens a real git repo. Walks history, extracts hunks,
runs `blame_file`.

### `ohara-parse`

`tree-sitter` extractors for Rust, Python, Java, and Kotlin.
Implements `SymbolSource` for HEAD-snapshot symbol extraction and
applies the AST sibling-merge chunker (Plan 3 / Track C) to keep
chunks under a 500-token budget. See
[Language support](./languages.md).

### `ohara-cli`

The `ohara` binary. One subcommand per file under
`src/commands/`: [`init`](../cli/init.md), [`index`](../cli/index.md),
[`query`](../cli/query.md), [`explain`](../cli/explain.md),
[`status`](../cli/status.md), [`update`](../cli/update.md). Each
command builds the same core/storage/embed/git/parse stack.

### `ohara-mcp`

The `ohara-mcp` binary. Hosts the `OharaService` with the
`find_pattern` and `explain_change` tools (see
[MCP tool reference](../tools/find_pattern.md)) and serves them over
stdio via [`rmcp`](https://crates.io/crates/rmcp).

### `tests/perf`

Out-of-band perf harness — a workspace member but not a published
crate. Used to capture the v0.6 baseline numbers (see
`docs/perf/v0.6-baseline.md`) and to A/B retrieval-quality tweaks
without polluting the main crates' `[dev-dependencies]`.

## Reading order

If you're new to the codebase, follow the data flow:

1. [Indexing & abort-resume](./indexing.md) — what `ohara index` does
   end-to-end.
2. [Storage schema](./storage.md) — what lands in SQLite.
3. [Retrieval pipeline](./retrieval-pipeline.md) — how a query
   becomes a ranked list of hits.
4. [Language support](./languages.md) — how symbols get extracted.
