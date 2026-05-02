# Indexing & abort-resume

`ohara index` is the only writer to the SQLite index. This page walks
through what it does end-to-end, the two fast-paths
(`--incremental` and `--force`), and the abort-resume contract.

## End-to-end

A full pass, in order:

1. **Resolve repo id.** Hash the canonical repo path + the SHA of the
   first commit on `HEAD`. Yields a stable `repo_id` used as the
   directory name under `$OHARA_HOME/`.
2. **Open / migrate the index.** `SqliteStorage::open` runs any
   pending refinery migrations (`migrations/V*.sql`) before returning.
3. **List new commits.** `CommitSource::list_commits(after =
   watermark)` walks `git log` from the storage watermark to `HEAD`.
   On a fresh index the watermark is `None` and the walker returns
   every commit.
4. **For each commit (batched by `--commit-batch`, default 512):**
    1. Extract hunks via `CommitSource::hunks_for_commit`.
    2. Embed `[commit_message, hunk_1.diff_text, …]` in a single
       `embed_batch` call.
    3. `put_commit` (commit row + `vec_commit` + `fts_commit`).
    4. `put_hunks` (hunk rows + `vec_hunk` + `fts_hunk_text`,
       DELETE-then-INSERT scoped by `commit_sha`).
    5. Every 100 commits, advance `repo.last_indexed_commit` and emit
       a `tracing::info!` progress event.
5. **Extract HEAD symbols.** `SymbolSource::extract_head_symbols`
   walks the working tree, parses each supported file with
   tree-sitter, runs the AST sibling-merge chunker, and produces one
   `Symbol` per chunk.
6. **`put_head_symbols`.** Replaces the entire `symbol` /
   `vec_symbol` / `fts_symbol` / `fts_symbol_name` content for the
   repo (HEAD is a snapshot, not history).
7. **Final watermark advance.** Set `last_indexed_commit` to the
   newest commit walked.

## `--incremental` fast path

Used by the [`ohara init`](../cli/init.md) post-commit hook and any
caller that wants a no-op re-index to be cheap.

Before booting the embedder (which costs hundreds of milliseconds
even when the model is cached), `ohara index --incremental` reads
`repo.last_indexed_commit` and compares to `HEAD`. If they match, it
prints `index up-to-date at <sha>` and returns immediately — no
embedder init, no walker boot, no SQLite write transaction.

When they don't match, the pass proceeds normally and walks just the
new commits.

## `--force` rebuild path

Clears existing HEAD symbol rows before the pass and re-extracts from
scratch. Used after upgrading to a new ohara that changed the AST
chunker (e.g. the v0.3 sibling-merge chunker would otherwise produce
duplicate symbols when run over a v0.2-era index).

`--force` only touches HEAD symbols. Commit and hunk history are
untouched — they're append-only and embed-stable. `--force` wins over
`--incremental` if both flags are set.

## Abort-resume contract

The watermark advances every 100 commits during the commit walk. That,
plus `put_hunks`'s DELETE-then-INSERT semantics, gives the contract:

- A Ctrl-C / kill / crash mid-walk loses **at most ~100 commits** of
  progress.
- Re-running `ohara index` after an abort re-does those ≤ 100 commits.
  The DELETE step in `put_hunks` clears any partially-written hunks
  for those SHAs first, so no duplicate rows accumulate.
- Anything outside the commit walk (HEAD-symbol extraction, the final
  watermark advance) is small enough to redo cleanly.

## Profiling

Pass `--profile` to dump a single-line JSON `PhaseTimings` blob on
stdout after the run. Captures per-phase wall-time
(`commit_walk_ms`, `diff_extract_ms`, `embed_ms`, `storage_write_ms`,
`head_symbols_ms`, …) and the hunk-text inflation diagnostic
(`total_diff_bytes / total_added_lines`). The numbers feed the v0.6
throughput baseline; see
`docs/perf/v0.6-baseline.md` for the template.

## v0.6 indexer knobs

A few v0.6 additions worth singling out — the full flag reference is
on [`ohara index`](../cli/index.md):

- **`--embed-provider {auto,cpu,coreml,cuda}`.** Picks the ONNX
  execution provider for the embedder. `auto` (default) chooses
  CoreML on Apple silicon, CUDA when `CUDA_VISIBLE_DEVICES` is set,
  and CPU otherwise. CoreML / CUDA require a feature-flagged
  build — see [Install → hardware acceleration](../install.md#build-with-hardware-acceleration);
  the published cargo-dist binaries are CPU-only.
- **`--resources {auto,conservative,aggressive}`.** A small lookup
  table that picks reasonable `--commit-batch` / `--threads` /
  `--embed-provider` defaults from the host's logical core count.
  Explicit flags always override the picked plan, so
  `--resources aggressive --commit-batch 256` is meaningful.
- **`--profile`.** Already covered above — emits the per-phase
  `PhaseTimings` JSON used by the throughput baseline.
- **Pinned progress bar.** The CLI now wires
  [`tracing-indicatif`](https://github.com/emersonford/tracing-indicatif)
  into its `tracing` subscriber, so the indexer's progress bar stays
  pinned to the bottom of the terminal while `tracing::info!` events
  stream above it. No more "log line scrolled the bar away."

## Known limits

`ohara index` is currently single-process. On large polyglot
codebases the embed phase saturates a few cores for a burst, then
drops to single-core for the SQLite/FTS5 tail — making this fast
without regressing retrieval quality is the
[v0.6 throughput RFC](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md).
