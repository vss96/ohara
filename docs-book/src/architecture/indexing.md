# Indexing & abort-resume

Two processes write the SQLite index: the `ohara index` CLI, and a
running `ohara serve` daemon for incremental work (see
[Worktrees and the daemon writer](#worktrees-and-the-daemon-writer)).
This page walks through what an index pass does end-to-end, the two
fast-paths (`--incremental` and `--force`), how linked git worktrees
share one index, and the abort-resume contract.

## End-to-end

A full pass, in order:

1. **Resolve repo identity.** A plain clone hashes its canonical path
   + the SHA of the first commit on `HEAD`. A linked git worktree
   resolves through git's `commondir` to its main clone and takes the
   main clone's id, so a clone and all its worktrees share one
   `repo_id` (the directory name under `$OHARA_HOME/`) and one
   `index.sqlite`. The worktree itself is identified by a worktree
   key: `''` for the main checkout, the name of the worktree's admin
   dir (`.git/worktrees/<name>`) for a linked one.
2. **Open / migrate the index.** `SqliteStorage::open` runs any
   pending refinery migrations (`migrations/V*.sql`) before returning.
3. **List new commits.** The walker starts at this worktree's `HEAD`
   and hides every indexed tip of every worktree (the `indexed_tip`
   table). What remains is exactly the history no worktree has
   indexed yet, so a commit shared by two worktrees is embedded once.
   On a fresh index there are no tips and the walker returns every
   commit. Commits that already have a `commit_record` row are skipped
   too (see [Abort-resume contract](#abort-resume-contract)).
4. **For each commit (batched by `--commit-batch`, default 512):**
    1. Extract hunks via `CommitSource::hunks_for_commit`.
    2. Embed `[commit_message, hunk_1.diff_text, …]` in a single
       `embed_batch` call.
    3. `put_commit` (commit row + `vec_commit` + `fts_commit`).
    4. `put_hunks` (hunk rows + `vec_hunk` + `fts_hunk_text`,
       DELETE-then-INSERT scoped by `commit_sha`).
    5. Every 100 commits, emit a `tracing::info!` progress event.
5. **Extract HEAD symbols (main checkout, or any worktree until main
   has a tip).** `SymbolSource::extract_head_symbols` walks the
   working tree, parses each supported file with tree-sitter, runs
   the AST sibling-merge chunker, and produces one `Symbol` per
   chunk. The head-snapshot symbol lane describes the main
   checkout's `HEAD`, so once the main checkout has a tip a linked
   worktree skips this step. Until then (a shared index first built
   from a linked worktree, or a bare repo with only linked
   worktrees) whichever worktree indexes writes it, so the lane is
   never empty.
6. **`put_head_symbols`.** Replaces the `symbol` / `vec_symbol` /
   `fts_symbol` / `fts_symbol_name` content for the repo (HEAD is a
   snapshot, not history) in one transaction, after extraction has
   finished: a failed extraction leaves the previous snapshot intact,
   and queries never see an empty lane mid-run. The main checkout's
   first pass replaces whatever a linked worktree wrote.
7. **Record the tip.** When every commit indexed cleanly, set this
   worktree's row in `indexed_tip` to the `HEAD` the pass started
   from (the walk in step 3 also starts from that commit, not from a
   `HEAD` that moved mid-pass). If the worktree's previous tip is not
   an ancestor of the new one (branch switch, `reset`, a slot reused
   for another run), the previous tip is kept as a *retired tip*
   first; see [Retired tips](#retired-tips). A pass with failed
   commits leaves the tip where it was, so the next pass retries
   them.

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

Run `ohara plan` to survey a repo's commit-share hotmap and print
suggested `.oharaignore` patterns. The planner runs a paths-only
libgit2 walk (seconds-to-minutes even on giant repos), groups commits
by top-level directory, and proposes ignoring high-share directories
outside a small documentation allowlist.

`ohara plan` is **print-only by default** — it never edits
`.oharaignore` unless you pass `--write`. With `--write`, the
suggestions are merged into a marker-fenced auto-generated section at
the repo root; user-added lines below the closing marker survive
re-runs (use `--write --replace` to overwrite the whole file). The
share-based heuristic is a hint, not a verdict — review the suggested
patterns before applying, especially on repos where the most-touched
top-level directory is the engine itself.

When a commit's changed paths are 100% ignored, the indexer skips it
entirely (no rows written). The worktree's tip still moves past it at
the end of the pass, so `--incremental` runs work normally.

## `--incremental` fast path

Used by the [`ohara init`](../cli/init.md) post-commit hook and any
caller that wants a no-op re-index to be cheap.

Before booting the embedder (which costs hundreds of milliseconds
even when the model is cached), `ohara index --incremental` counts the
commits reachable from `HEAD` that no indexed tip covers. If there are
none, it prints `index up-to-date at <sha>` and returns immediately —
no embedder init, no walker boot, no SQLite write transaction.

When there is work, the pass goes to a running daemon if there is one
(next section), and otherwise proceeds in-process and walks just the
new commits.

## Worktrees and the daemon writer

A clone and its linked worktrees share one index (see step 1 above).
Each worktree has its own row in `indexed_tip`; the main checkout's
key is `''`. Indexing a worktree embeds only the commits no other
worktree has indexed, and queries from a worktree only see commits
its own `HEAD` can reach (see
[Retrieval pipeline → worktree visibility](./retrieval-pipeline.md#worktree-visibility-filter)).

When an `ohara serve` daemon built from the same ohara version is
already running, `ohara index --incremental` hands the pass to it
instead of indexing in-process: one writer per store, and the
daemon's warm embedder is reused. The CLI never spawns a daemon for
this. It falls back to indexing in-process only when no such daemon
is registered, the daemon can't be reached, or the daemon replies
that the repo has no index yet. Once the daemon has accepted the
request, a timeout or a `needs rebuild` reply is reported as an
error and nothing runs in-process, because the daemon may still be
writing. Runs that tune the indexer (`--threads`, `--workers`,
`--commit-batch`, `--embed-batch`, `--embed-cache`, a non-CPU
`--embed-provider`), `--force` and `--rebuild` always run in-process.

The daemon's index work:

- **`EnsureIndexed { sha }`** — block until `sha` is covered by some
  indexed tip, indexing this worktree's `HEAD` if needed. This is
  what `ohara index --incremental` calls.
- **`IndexTip`** — queue a background pass for this worktree.
  Repeated requests for a worktree coalesce while it waits.
- **`PruneUnreachable`** — delete indexed commits that no ref and no
  worktree `HEAD` (detached ones included) reaches any more, then
  tidy the tips: a retired tip is dropped once nothing reaches it; a
  linked worktree that no longer exists has its tip retired while a
  ref still reaches it, and dropped otherwise. The main tip is never
  dropped.

The main checkout is never treated as covered before it has its own
tip: its first `EnsureIndexed` (or `ohara index --incremental`) runs a
pass that records the tip and takes over the HEAD symbols, even when
linked worktrees' tips already reach its `HEAD`.

### Retired tips

A worktree's commits stay in the index after the worktree moves to a
commit that does not descend from its old tip, so the old tip keeps
counting as a tip: it is stored in `indexed_tip` under the reserved key
`<worktree>/<sha>` (git worktree names never contain `/`, so it cannot
collide with a real worktree). Retired tips are hidden by the walk,
count for coverage, and — the reason they exist — make retrieval apply
the visibility filter whenever one is not reachable from the querying
`HEAD`. Without them a reused slot whose new run was merged into main
would make every tip visible from main, the filter would be skipped,
and the old run's unmerged commits would leak into main's results.
They are not worktrees: `status`, a worktree's own-tip lookup and
`_meta.index_status` ignore them. Prune drops a retired tip once no
ref reaches it (its commits are pruned in the same pass).

Jobs are serialised per repository. The daemon never builds an index
from scratch (run `ohara index` once first), and it refuses to write
unless the stored index is fully `compatible` *and* its own loaded
embedder matches the stored model and dimension, so it never appends
mismatched vectors.

## `--force` rebuild path

Clears existing HEAD symbol rows before the pass and re-extracts from
scratch. Used after upgrading to a new ohara that changed the AST
chunker (e.g. the v0.3 sibling-merge chunker would otherwise produce
duplicate symbols when run over a v0.2-era index).

`--force` only touches HEAD symbols. Commit and hunk history are
untouched — they're append-only and embed-stable. `--force` wins over
`--incremental` if both flags are set. In a linked worktree `--force`
leaves the HEAD symbols alone (they follow the main checkout) and
says so; run it from the main checkout to refresh them.

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

A worktree's tip is recorded only at the end of a clean pass. Resume
safety comes from two other mechanisms:

- **Per-commit skip.** The walker drops any commit that already has a
  `commit_record` row (a sub-millisecond PK lookup), so re-running
  `ohara index` after a Ctrl-C / kill / crash re-walks the history
  since the last tip but re-embeds only the commits that never landed.
- **DELETE-then-INSERT.** `put_hunks` clears any partially-written
  hunks for a SHA first, so a commit interrupted mid-write is redone
  without duplicate rows.

Anything outside the commit walk (HEAD-symbol extraction, recording
the tip) is small enough to redo cleanly.

The per-commit skip also matters on merge-heavy histories:
`git2::Revwalk::hide(tip)` only excludes the tip and its ancestors,
so commits reachable via a different parent path — feature-branch
merges, octopus merges, history rewrites — would otherwise be
re-walked and re-embedded even though they're already in the index.
See
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
  execution provider for the embedder. `auto` (default) chooses CUDA
  when `CUDA_VISIBLE_DEVICES` is set and CPU otherwise. `coreml`
  (opt-in, v0.11) runs the fixed-shape fp32 BGE-small on the Apple
  GPU+Neural Engine at ~3× CPU throughput — built into the released
  macOS binary; see [Install → hardware acceleration](../install.md#build-with-hardware-acceleration).
  CUDA requires a feature-flagged source build.
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

## Parallel commit pipeline (`--workers`)

`ohara index` runs a multi-worker pipeline by default:

- A walker task enumerates HEAD-reachable commits.
- N worker tasks each pull a commit and run the full pipeline
  (hunk_chunk → attribute → embed → persist) end-to-end.
- A bounded mpsc channel (capacity = N) provides backpressure.

Each commit gets a deterministic ULID derived from `(commit_time,
commit_sha)`. Persistence is order-free; the read path recovers
chronological order via `ORDER BY ulid`. Resume falls back to plan-9's
`commit_exists` skip — already-indexed commits are dropped from the
walker output.

Default: `--workers $(num_cpus)`. Use `--workers 1` for the serial
path (matches today's behavior; useful for debugging). The big speedup
shows up when the chunk-embed cache (`--embed-cache=semantic|diff`,
plan-27) is warm — most chunks then become cache lookups and parse
becomes the dominant per-commit cost, fanning out across workers.

## Known limits

- **Failed passes leave untipped commits.** A pass with failed
  commits stores the rest but doesn't record a tip. When every tip is
  visible from a querying worktree's `HEAD`, retrieval skips the
  visibility filter, so those stored commits can show up in results
  from other worktrees until the next clean pass or a prune. (A
  worktree that *moves* off indexed commits is covered: its old tip is
  kept as a [retired tip](#retired-tips). Commits that never got a tip
  at all are not.)
- **No cross-process writer lock.** The daemon serialises its own
  jobs, but a CLI `--force` or `--rebuild`, or a second daemon
  briefly overlapping the first during shutdown or a version upgrade,
  can write while the daemon does. SQLite keeps the data consistent;
  expect `SQLITE_BUSY` errors or duplicated work.
- **Unopenable worktrees are skipped when pruning.** A worktree whose
  directory git can't open is skipped with a warning when computing
  reachability, so commits only that worktree reaches can be pruned.

`ohara index` is currently single-process. On large polyglot
codebases the embed phase saturates a few cores for a burst, then
drops to single-core for the SQLite/FTS5 tail — making this fast
without regressing retrieval quality is the
[v0.6 throughput RFC](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md).
