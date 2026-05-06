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

## Chunk-level embed cache (`--embed-cache`)

`ohara index` can be told to cache embeddings keyed by the content
the embedder consumes, so identical chunk content costs one embed
call across the entire history rather than one per occurrence.

Three modes:

- `off` (default) — no cache; today's behavior.
- `semantic` — cache keyed by `sha256(commit_msg + diff_text)`;
  embedder input unchanged. Hit rate is driven by exact
  `(message, diff)` repeats — cherry-picks, reverts. Conservative.
- `diff` — cache keyed by `sha256(diff_text)`; **embedder input
  changes to `diff_text` only** (commit message dropped from the
  vector lane). Hit rate is much higher (vendor refreshes, mass
  renames). The vector lane specialises in diff-similarity; commit
  messages remain indexed via the existing `fts_hunk_semantic` BM25
  lane.

`off` and `semantic` are vector-equivalent (both embed the same
input). `diff` produces a different vector lane; switching into or
out of it requires `--rebuild`.

The cache lives in the same SQLite DB as `vec_hunk` and is bounded
by `unique(content_hash, embed_model)`. No eviction in v1.

Usage:

```
ohara index --embed-cache semantic ~/code/big-repo
ohara status ~/code/big-repo   # shows embed_cache: semantic (… KB)
```

## Path-aware indexing — `.oharaignore`

`ohara` consults a layered ignore filter at index time. Three sources
are merged, with the user layer winning so `!negate` patterns work:

1. **Built-in defaults** (compiled into `ohara-core`) — lockfiles,
   `node_modules/`, `target/`, `vendor/`, `dist/`, etc.
2. **`.gitattributes`** — paths flagged `linguist-generated=true` or
   `linguist-vendored=true`.
3. **`.oharaignore`** at repo root — gitignore-syntax, team-shared.

Run `ohara plan` to survey a repo's commit-share hotmap and write a
suggested `.oharaignore`. The planner runs a paths-only libgit2 walk
(seconds-to-minutes even on giant repos), groups commits by top-level
directory, and proposes ignoring high-share directories outside a
small documentation allowlist.

When a commit's changed paths are 100% ignored, the indexer skips it
entirely (no rows written) but advances `last_indexed_commit` past it,
so `--incremental` runs work normally.

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

## Index compatibility (v0.7)

A v0.7 index records per-component metadata in the `index_metadata`
table at the end of every successful pass: embedding model, embedding
dimension, reranker model, AST chunker version, semantic-text
version, schema version, and one parser version per language. On
every CLI / MCP invocation the runtime builds the same snapshot from
constants and compares the two. The verdict is one of:

| Verdict | Meaning | What it gates |
|---|---|---|
| `compatible` | Every recorded component matches the binary. | Nothing — proceed. |
| `query-compatible, refresh recommended` | A *derived* component bumped (chunker, parser, semantic-text, reranker). KNN still works because the vectors are unchanged; the derived rows are stale. | `ohara index --force` to refresh derived rows. |
| `needs rebuild` | A *vector-affecting* component differs (embedding model, dimension, schema). KNN against this index would return wrong results. | `ohara index --rebuild` to drop and rebuild. `find_pattern` MCP refuses to run; `explain_change` continues because blame doesn't use vectors. |
| `unknown` | Pre-v0.7 index, or freshly migrated before any v0.7+ pass wrote metadata. | `ohara index --force` records current versions; future runs become `compatible`. |

`--force` vs `--rebuild`:

- `--force` refreshes derived symbol/chunker outputs without touching
  the commit/hunk/vector history. Cheap; safe to re-run.
- `--rebuild` deletes the entire index and rebuilds from scratch.
  Slow and destructive; requires `--yes` to confirm. Use only when
  the verdict is `needs rebuild`.

Component-version constants live in their owning crates:
[`ohara_parse::CHUNKER_VERSION`](https://github.com/vss96/ohara/blob/main/crates/ohara-parse/src/lib.rs)
+ `parser_versions()`, `ohara_embed::DEFAULT_MODEL_ID` /
`DEFAULT_DIM` / `DEFAULT_RERANKER_ID`, and
`ohara_core::index_metadata::{SCHEMA_VERSION, SEMANTIC_TEXT_VERSION}`.
Bump a constant when its owning code's output semantics change in a
way that would invalidate previously-indexed rows.

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

The watermark is a single SHA, so on resume the indexer also
short-circuits per-commit when `commit_record` already has a row for
the SHA being walked (v0.6.3). This matters on merge-heavy histories:
`git2::Revwalk::hide(watermark)` only excludes the watermark and its
strict ancestor chain, so commits reachable via a different parent
path — feature-branch merges, octopus merges, history rewrites — would
otherwise be re-walked and re-embedded even though they're already in
the index. A sub-millisecond PK lookup avoids that wasted embedder
cost. See
[`docs/superpowers/specs/2026-05-02-ohara-v0.6.3-resume-skip-rfc.md`](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-02-ohara-v0.6.3-resume-skip-rfc.md)
for the design.

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
