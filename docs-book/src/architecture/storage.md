# Storage schema

ohara persists everything in a single SQLite file at
`$OHARA_HOME/<repo-id>/index.sqlite` (default `~/.ohara/`). The file
loads three SQLite extensions:

- [`sqlite-vec`](https://github.com/asg017/sqlite-vec) for the
  `vec_*` virtual tables (k-NN over float32 embeddings).
- FTS5 (built into SQLite) for the `fts_*` virtual tables.
- Refinery for forward migrations under
  `crates/ohara-storage/migrations/V*.sql`.

## Tables

### V1 — initial schema

| Table | Kind | Purpose |
|-------|------|---------|
| `repo` | normal | One row per indexed repo: id, path, first commit, watermark, `indexed_at`, schema version. |
| `commit_record` | normal | One row per commit: SHA, parent SHA, merge flag, unix ts, author, message. Indexed on `ts`. |
| `file_path` | normal | Interned file path strings + detected language + active flag. |
| `symbol` | normal | HEAD-snapshot symbols: file, kind, name, qualified_name, line span, blob SHA, `source_text`. |
| `hunk` | normal | One row per (commit, file, hunk): commit SHA, file path id, change kind, raw `diff_text`. Indexed on `(file_path_id, commit_sha)` and `commit_sha`. |
| `blob_cache` | normal | Tracks which blob SHAs have been embedded by which model — keeps re-indexes idempotent. |
| `vec_hunk` | `vec0` | sqlite-vec table: `hunk_id → diff_emb FLOAT[384]` (BGE-small). |
| `vec_commit` | `vec0` | `commit_sha → message_emb FLOAT[384]`. |
| `vec_symbol` | `vec0` | `symbol_id → source_emb FLOAT[384]`. |
| `fts_commit` | FTS5 | BM25 over commit messages. |
| `fts_symbol` | FTS5 | BM25 over `(qualified_name, source_text)`. |

### V2 — three-lane retrieval (v0.3)

| Table / column | Kind | Purpose |
|----------------|------|---------|
| `symbol.sibling_names` | new column | JSON array of sibling-symbol names from the AST sibling-merge chunker. Defaults to `'[]'` for v0.2-era rows. |
| `fts_hunk_text(hunk_id, content)` | FTS5 | BM25 lane over raw hunk body — feeds the second retrieval lane in [`find_pattern`](./retrieval-pipeline.md). |
| `fts_symbol_name(symbol_id, kind, name, sibling_names)` | FTS5 | BM25 lane over tree-sitter symbol names — feeds the third retrieval lane. |

V2 backfills both new FTS tables from existing `hunk` and `symbol`
rows on first run, so a v0.3 binary opening a v0.2 index is searchable
immediately without re-embedding.

### V3 — index metadata (v0.7, plan 13)

| Table / column | Kind | Purpose |
|----------------|------|---------|
| `index_metadata(repo_id, component, version, value_json, recorded_at)` | new table | Per-component record of how the index was built. Lets the runtime detect when an old index is incompatible with the current binary's embedder / chunker / parser / semantic-text versions and decide between "fine", "refresh recommended", and "rebuild required". |

Initial component keys: `schema`, `embedding_model`,
`embedding_dimension`, `reranker_model`, `chunker_version`,
`semantic_text_version`, plus one parser key per language
(`parser_rust`, `parser_python`, `parser_java`, `parser_kotlin`).

V3 backfills only `(repo_id, 'schema', '3', …)` for every existing
repo. Other components stay absent — the runtime reports them as
`Unknown` rather than guessing what version the prior pass used. A
subsequent index pass writes the current binary's values into the
table at successful completion.

## Idempotency

Indexing is structured so any pass can be repeated without
duplicating data:

- **`put_commit`** — `INSERT OR REPLACE` on the commit record + the
  `vec_commit` and `fts_commit` rows.
- **`put_hunks`** — DELETE-then-INSERT scoped by `commit_sha`. Any
  previously-written hunks (and their `vec_hunk` / `fts_hunk_text`
  rows) for that SHA are cleared first, so a re-index of the same
  commit produces exactly the same row count.
- **`put_head_symbols`** — replaces the entire `symbol` /
  `vec_symbol` / `fts_symbol` / `fts_symbol_name` content for the
  repo. HEAD is a snapshot, not history; partial updates would leak
  stale symbols across commits.

## Watermark + abort safety

`repo.last_indexed_commit` is the watermark. The `Indexer` advances it
**every 100 commits** during the walk (and once at the end of the
pass). Combined with `put_hunks`'s DELETE-then-INSERT semantics, the
worst-case cost of a Ctrl-C / kill / crash mid-walk is re-doing
~100 commits — already-written hunks are cleared and re-inserted
identically, so no duplicate rows accumulate.

Schema version (`repo.schema_version`) is bumped when migrations land.
A v0.3 binary on a v0.2 index runs migration V2; a v0.2 binary on a
v0.3 index aborts with a clean error rather than risking a downgrade.

## Source layout

Storage code lives under `crates/ohara-storage/src/`. As of v0.6 the
crate is split into two submodules: `tables/` (one file per table —
`commit`, `hunk`, `symbol`, `repo`, `blob_cache`, plus the FTS / vec
mirror tables) and `codec/` (row codecs like `change_kind` and
`file_path` interning, factored out of the table modules so the
mapping is shared and unit-tested in one place). Internal-only — no
public API change.

## Why SQLite

A single static binary that survives `cp index.sqlite` and `rsync`
across machines was the constraint. SQLite + sqlite-vec + FTS5 hit
that without a daemon, and the DB is small enough on real-world repos
(~100–200 MB on QuestDB-class history) that backup is just a file
copy.
