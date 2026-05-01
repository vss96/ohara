# `ohara index`

Walk a repo's git history, embed every commit's diff hunks, and
extract HEAD-snapshot symbols into the local SQLite index. Idempotent
and abort-safe — see [Indexing & abort-resume](../architecture/indexing.md)
for the full state machine.

## Usage

```
ohara index [PATH] [--incremental] [--force] [--commit-batch N] \
            [--threads N] [--no-progress] [--profile]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo. |
| `--incremental` | off | Skip the indexer (and embedder init) when the storage watermark already points at HEAD. Used by the post-commit hook to make no-op re-indexes nearly free. |
| `--force` | off | Clear existing HEAD symbol rows and re-extract from scratch. Used after upgrades that change the AST chunker. Wins over `--incremental` if both are set; commit/hunk history is untouched. |
| `--commit-batch` | `512` | Commits per storage transaction. Smaller = less peak RAM and more frequent fsyncs; larger = faster but uses more memory. |
| `--threads` | `0` | Cap the embedder's ONNX runtime to this many threads (`0` = let `ort` decide, typically CPU count). Useful on shared dev machines. |
| `--no-progress` | off | Disable the progress bar even when stderr is a TTY. Structured `tracing::info!` events still fire every 100 commits. |
| `--profile` | off | Emit a single-line JSON `PhaseTimings` blob on stdout after the run finishes (per-phase wall time + hunk-text inflation). Used by the v0.6 throughput baseline. |

## Examples

First-time index of the current repo:

```sh
ohara index
```

Hook-style re-index — fast no-op when HEAD is already indexed:

```sh
ohara index --incremental
```

Force a HEAD-symbol rebuild after upgrading to a new ohara that
changed the chunker:

```sh
ohara index --force
```

Cap embedder threads on a shared box, larger batches for speed:

```sh
ohara index --threads 4 --commit-batch 1024
```

Capture per-phase timings for performance work:

```sh
ohara index --profile | tail -1 | jq .
```

## Output

A summary line on stdout:

```
indexed: 132 new commits, 487 hunks, 1204 HEAD symbols
```

Plus structured tracing events on stderr (drive verbosity with
`RUST_LOG`, e.g. `RUST_LOG=info`). With `--profile`, a JSON line
follows the summary:

```json
{"commit_walk_ms":42,"diff_extract_ms":318,"tree_sitter_parse_ms":0,"embed_ms":1820,"storage_write_ms":210,"fts_insert_ms":0,"head_symbols_ms":540,"total_diff_bytes":482312,"total_added_lines":1842}
```

## Resume safety

Killed mid-walk? The watermark advances every 100 commits inside the
indexer. Worst case on resume is re-doing ~100 commits — `put_hunks`
clears any previously-written hunks for those SHAs first, so duplicates
never accumulate. See [Indexing & abort-resume](../architecture/indexing.md).
