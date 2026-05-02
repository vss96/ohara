# `ohara index`

Walk a repo's git history, embed every commit's diff hunks, and
extract HEAD-snapshot symbols into the local SQLite index. Idempotent
and abort-safe — see [Indexing & abort-resume](../architecture/indexing.md)
for the full state machine.

## Usage

```
ohara index [PATH] [--incremental] [--force] [--commit-batch N] \
            [--threads N] [--no-progress] [--profile] \
            [--embed-provider {auto,cpu,coreml,cuda}] \
            [--resources {auto,conservative,aggressive}]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo. |
| `--incremental` | off | Skip the indexer (and embedder init) when the storage watermark already points at HEAD. Used by the post-commit hook to make no-op re-indexes nearly free. |
| `--force` | off | Clear existing HEAD symbol rows and re-extract from scratch. Used after upgrades that change the AST chunker. Wins over `--incremental` if both are set; commit/hunk history is untouched. |
| `--commit-batch` | from `--resources` | Commits per storage transaction. Smaller = less peak RAM and more frequent fsyncs; larger = faster but uses more memory. When unset, `--resources` picks a value from host core count. |
| `--threads` | from `--resources` | Cap the embedder's ONNX runtime to this many threads (`0` = let `ort` decide, typically CPU count). When unset, `--resources` picks a value from host core count. |
| `--no-progress` | off | Disable the progress bar even when stderr is a TTY. Structured `tracing::info!` events still fire every 100 commits. |
| `--profile` | off | Emit a single-line JSON `PhaseTimings` blob on stdout after the run finishes (per-phase wall time + hunk-text inflation). Used by the v0.6 throughput baseline. |
| `--embed-provider` | from `--resources` | ONNX execution provider for the embedder: `auto` (default — CoreML on Apple silicon, CUDA when `CUDA_VISIBLE_DEVICES` is set, else CPU), `cpu`, `coreml`, or `cuda`. CoreML / CUDA require a feature-flagged build; see [Install → hardware acceleration](../install.md#build-with-hardware-acceleration). |
| `--resources` | `auto` | Resource intensity policy. `auto` picks `--commit-batch` / `--threads` / `--embed-provider` from host core count. `conservative` halves the picked batch + thread count; `aggressive` doubles them. Explicit flags always override the picked plan. |

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

Run the indexer with hardware acceleration on Apple silicon (requires
a `--features coreml` build):

```sh
ohara index --embed-provider coreml
```

Trade off resource intensity against the rest of the box —
`conservative` halves batch + threads, `aggressive` doubles them:

```sh
ohara index --resources conservative
ohara index --resources aggressive --commit-batch 1024   # explicit flag still wins
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
