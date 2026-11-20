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
| `--incremental` | off | Skip the indexer (and embedder init) when every commit reachable from HEAD is already covered by an indexed tip. Used by the post-commit hook to make no-op re-indexes nearly free. When there is work and an `ohara serve` daemon of the same version is running, the pass runs in the daemon (see [Worktrees](#worktrees)). |
| `--force` | off | Clear existing HEAD symbol rows and re-extract from scratch. Used after upgrades that change the AST chunker. Wins over `--incremental` if both are set; commit/hunk history is untouched. In a linked worktree it leaves HEAD symbols alone (they follow the main checkout). |
| `--rebuild` | off | **Destructive.** Delete the entire index for this repo and rebuild from scratch. Stronger than `--force` (which only refreshes HEAD-symbol rows). Used when `ohara status` reports `compatibility: needs rebuild` (the binary's embedder dimension or model differs from what the index was built with). Requires `--yes` to confirm; conflicts with `--incremental` and `--force`. |
| `--yes` | off | Confirm a destructive operation. Currently only valid alongside `--rebuild`. |
| `--commit-batch` | from `--resources` | Commits per storage transaction. Smaller = less peak RAM and more frequent fsyncs; larger = faster but uses more memory. When unset, `--resources` picks a value from host core count. |
| `--threads` | from `--resources` | Cap the embedder's ONNX runtime to this many threads (`0` = let `ort` decide, typically CPU count). When unset, `--resources` picks a value from host core count. |
| `--no-progress` | off | Disable the progress bar even when stderr is a TTY. Structured `tracing::info!` events still fire every 100 commits. |
| `--profile` | off | Emit a single-line JSON `PhaseTimings` blob on stdout after the run finishes (per-phase wall time + hunk-text inflation). Used by the v0.6 throughput baseline. |
| `--embed-provider` | from `--resources` | ONNX execution provider for the embedder: `auto` (default — CUDA when `CUDA_VISIBLE_DEVICES` is set, else CoreML on a CoreML-capable macOS build, else CPU), `cpu`, `coreml`, or `cuda`. `coreml` (Apple Silicon) runs the fixed-shape fp32 BGE-small on the GPU+Neural Engine — ~3× CPU throughput; released macOS binaries ship with it enabled, so `auto` prefers it there for indexing. First use downloads the fp32 model (~130MB) and each indexing run pays a one-time ~30s CoreML compile (queries always embed on CPU). Existing indexes stay compatible. CUDA requires a feature-flagged build; see [Install → hardware acceleration](../install.md#build-with-hardware-acceleration). |
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

## Worktrees

A git clone and all of its linked worktrees (`git worktree add`) share
one index. Running `ohara index` inside a linked worktree resolves to
the main clone's index, embeds only the commits no other worktree has
indexed yet, and records the worktree's own indexed tip. Queries from
each worktree only see commits its own `HEAD` reaches.

- The head-snapshot symbol lane describes the main checkout. Once the
  main checkout has been indexed, indexing from a linked worktree
  never extracts or replaces HEAD symbols, and `--force` there prints
  `HEAD symbols follow the main worktree; not clearing them here`
  instead of clearing them. Until then (the shared index was first
  built from a linked worktree) linked worktrees write them, and the
  main checkout's first `ohara index --incremental` always runs a pass
  that records its tip and takes the HEAD symbols over.
- `--rebuild` inside a linked worktree deletes the index shared by the
  main clone and every linked worktree. It prints
  `warning: --rebuild from linked worktree '<name>' drops the index shared by all worktrees of this clone`
  on stderr before deleting.
- `--incremental` runs in a running same-version `ohara serve` daemon
  when there is one, so the daemon stays the single writer and its
  warm embedder is reused. `ohara index` never starts a daemon for
  this. It indexes in-process when no daemon is running, the daemon
  can't be reached, or the repo has no index yet. If the daemon
  accepted the request and then timed out or replied `needs rebuild`,
  the command fails instead of indexing in-process. Tuning flags
  (`--threads`, `--workers`, `--commit-batch`, `--embed-batch`,
  `--embed-cache`, a non-CPU `--embed-provider`), `--force` and
  `--rebuild` always run in-process.

See [Worktrees and the daemon writer](../architecture/indexing.md#worktrees-and-the-daemon-writer)
for the details and known limits.

## Interactive walkthrough

Prefer to be guided? `ohara index -i` (or `--interactive`) launches a
short wizard instead of asking you to remember the flags above. It
needs an attended terminal — piping or redirecting stdin makes it exit
with an error rather than hang. Move with ↑/↓ and choose with Enter;
Esc or Ctrl-C cancels without indexing.

The screens below are illustrative (your terminal renders colour and a
highlighted row). The wizard only lists providers your binary can
actually run, so the exact options vary by build and host — the example
is a CoreML-enabled macOS build.

**1. Embedding provider**

```text
? Embedding provider ›
❯ Auto (recommended) — resolves to CoreML on this host
  CPU
  CoreML — ~3x faster on Apple Silicon; first run downloads ~130MB and pays a one-time ~30s compile
```

When a provider is unavailable — a build without `--features coreml`,
or CUDA off a non-NVIDIA host — it is omitted and a one-line note
explains why (printed just above the menu):

```text
CoreML hidden: this binary was built without `--features coreml`.
```

**2. Resource intensity**

```text
? Resource intensity ›
❯ Auto (recommended)
  Conservative — halve batch + threads, yield to other work
  Aggressive — double batch + threads, maximize throughput
```

**3. Index mode**

```text
? Index mode ›
❯ Standard — index new commits
  Incremental — skip entirely if HEAD already indexed
  Force — re-extract HEAD symbols (after a chunker change)
  Rebuild — delete & rebuild the whole index from scratch
```

**Rebuild** is destructive, so it asks once more before arming
`--rebuild --yes`; declining drops you back to a Standard run:

```text
? Rebuild deletes and recreates the entire index for /path/to/repo. Continue? (y/N)
```

**4. Advanced knobs (optional)**

The common path stops here — answer **no** to run with sensible
defaults. Answer **yes** to tune threads, workers, batch sizes, the
embed-cache mode, and the progress/profile switches:

```text
? Configure advanced knobs? (y/N)
```

If you opt in, the numeric prompts accept a number, or blank / `auto`
to leave them unset (so `--resources` still picks them):

```text
? threads (blank or 'auto' for default): 4
? workers (blank or 'auto' for default): auto
? commit-batch (blank or 'auto' for default):
? embed-batch (blank or 'auto' for default): 256
? Embed cache mode ›
❯ off
  semantic
  diff
? Disable the progress bar? (y/N)
? Emit per-phase --profile JSON? (y/N)
```

**5. Preview & run**

Before anything runs, the wizard prints the exact command your answers
map to and asks for a final confirmation:

```text
Equivalent command:
  ohara index --embed-provider coreml --resources aggressive
? Run now? (Y/n)
```

Answer **yes** to index right away — the same run you'd get by typing
that command. Answer **no** and the wizard simply prints the command
(useful to copy into a script or a hook) and exits without indexing.

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

Killed mid-walk? Re-run the same command. The walk skips every commit
already stored, so only the commits that never landed are embedded
again, and `put_hunks` clears any partially-written hunks for a SHA
first, so duplicates never accumulate. The worktree's indexed tip is
recorded only at the end of a clean pass. See [Indexing & abort-resume](../architecture/indexing.md).
