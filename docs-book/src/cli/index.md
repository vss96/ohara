# `ohara index`

Walk a repo's git history, embed every commit's diff hunks, and
extract HEAD-snapshot symbols into the local SQLite index. Idempotent
and abort-safe — see [Indexing & abort-resume](../architecture/indexing.md)
for the full state machine.

## Usage

```
ohara index [PATH] [-i | --interactive] [--incremental] [--force] \
            [--rebuild --yes] [--commit-batch N] [--threads N] \
            [--no-progress] [--profile] \
            [--embed-provider {auto,cpu,coreml,cuda}] \
            [--resources {auto,conservative,aggressive}]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo. |
| `-i`, `--interactive` | off | Launch a guided wizard that prompts for embedding provider, resource intensity, index mode, and (opt-in) advanced knobs, previews the equivalent command, then runs it. Requires a TTY. Other tuning flags are ignored when `-i` is set — the wizard owns the tuning. |
| `--incremental` | off | Skip the indexer (and embedder init) when the storage watermark already points at HEAD. Used by the post-commit hook to make no-op re-indexes nearly free. |
| `--force` | off | Clear existing HEAD symbol rows and re-extract from scratch. Used after upgrades that change the AST chunker. Wins over `--incremental` if both are set; commit/hunk history is untouched. |
| `--rebuild` | off | **Destructive.** Delete the entire index for this repo and rebuild from scratch. Stronger than `--force` (which only refreshes HEAD-symbol rows). Used when `ohara status` reports `compatibility: needs rebuild` (the binary's embedder dimension or model differs from what the index was built with). Requires `--yes` to confirm; conflicts with `--incremental` and `--force`. |
| `--yes` | off | Confirm a destructive operation. Currently only valid alongside `--rebuild`. |
| `--commit-batch` | from `--resources` | Commits per storage transaction. Smaller = less peak RAM and more frequent fsyncs; larger = faster but uses more memory. When unset, `--resources` picks a value from host core count. |
| `--threads` | from `--resources` | Cap the embedder's ONNX runtime to this many threads (`0` = let `ort` decide, typically CPU count). When unset, `--resources` picks a value from host core count. |
| `--no-progress` | off | Disable the progress bar even when stderr is a TTY. Structured `tracing::info!` events still fire every 100 commits. |
| `--profile` | off | Emit a single-line JSON `PhaseTimings` blob on stdout after the run finishes (per-phase wall time + hunk-text inflation). Used by the v0.6 throughput baseline. |
| `--embed-provider` | from `--resources` | ONNX execution provider for the embedder: `auto` (default — CUDA when `CUDA_VISIBLE_DEVICES` is set, else CPU), `cpu`, `coreml`, or `cuda`. `coreml` (opt-in, Apple Silicon) runs the fixed-shape fp32 BGE-small on the GPU+Neural Engine — ~3× CPU throughput; released macOS binaries ship with it enabled. First use downloads the fp32 model (~130MB) and each run pays a one-time ~30s CoreML compile. Existing indexes stay compatible. CUDA requires a feature-flagged build; see [Install → hardware acceleration](../install.md#build-with-hardware-acceleration). |
| `--resources` | `auto` | Resource intensity policy. `auto` picks `--commit-batch` / `--threads` / `--embed-provider` from host core count. `conservative` halves the picked batch + thread count; `aggressive` doubles them. Explicit flags always override the picked plan. |

## Examples

First-time index of the current repo:

```sh
ohara index
```

Not sure which provider or knobs to use? Launch the interactive wizard:

```sh
ohara index -i
```

It walks you through provider (CoreML/CPU/CUDA — only the ones this
build supports), resource intensity, and index mode, shows the
equivalent `ohara index …` command, and runs it on confirm.

Hook-style re-index — fast no-op when HEAD is already indexed:

```sh
ohara index --incremental
```

Force a HEAD-symbol rebuild after upgrading to a new ohara that
changed the chunker:

```sh
ohara index --force
```

Full rebuild after an embedder/dimension change (`status` says
`needs rebuild`):

```sh
ohara index --rebuild --yes
```

Cap embedder threads on a shared box, larger batches for speed:

```sh
ohara index --threads 4 --commit-batch 1024
```

Capture per-phase timings for performance work:

```sh
ohara index --profile | tail -1 | jq .
```

Run the indexer with hardware acceleration on Apple silicon (released
macOS binaries have CoreML support built in; source builds need
`--features coreml`):

```sh
ohara index --embed-provider coreml
```

This routes embedding through the fixed-shape fp32 BGE-small on the
GPU+Neural Engine (~3× CPU throughput on an M4 Pro — see
`docs/perf/v0.11-coreml-fixed-shape.md` in the repo). The fp32 and
quantized models share one vector space, so you can mix providers
across passes without rebuilding.

Trade off resource intensity against the rest of the box —
`conservative` halves batch + threads, `aggressive` doubles them:

```sh
ohara index --resources conservative
ohara index --resources aggressive --commit-batch 1024   # explicit flag still wins
```

## Output

A summary block on stdout — header line plus a per-phase bar chart
sorted by descending wall-time, so the dominant stage leads:

```
indexed in 8.4s — 132 commits, 487 hunks, 1204 HEAD symbols

  embed     6.1s  ████████████████████████████████   73%
  storage   1.2s  ██████                             14%
  diff      0.6s  ███                                 7%
  parse     0.3s  ██                                  4%
  symbols   0.1s  █                                   1%
  fts       100ms                                    <1%
```

Phases with zero recorded ms are omitted. Percentages are anchored to
the wall-clock total.

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
