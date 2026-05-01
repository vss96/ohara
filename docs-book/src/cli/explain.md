# `ohara explain`

Run an `explain_change` query from the command line. Returns the same
JSON envelope as the MCP tool (see
[`explain_change`](../tools/explain_change.md)) so the result is
pipeable into `jq`.

## Usage

```
ohara explain <FILE> [PATH] [--lines START:END] [--k N] [--no-diff]
```

| Flag | Default | Description |
|------|---------|-------------|
| `FILE` (positional) | required | Repo-relative path to the file to explain. |
| `PATH` (positional) | `.` | Path to the repo. |
| `--lines` | full file | Line range as `START:END` (1-based, inclusive). Either bound may be omitted — `:42` starts at line 1, `10:` runs to end-of-file. Omit `--lines` entirely to explain the whole file. |
| `-k`, `--k` | `5` | Number of commits to return; clamped to `1..=20`. |
| `--no-diff` | off | Suppress diff excerpts in the output (only blame attribution and metadata). |

## Examples

Explain lines 40–60 of `src/auth.rs` with the top-3 contributing
commits:

```sh
ohara explain src/auth.rs --lines 40:60 --k 3
```

Explain the whole file, no diff excerpts:

```sh
ohara explain src/auth.rs --no-diff
```

Open-ended range — line 100 to end of file:

```sh
ohara explain src/auth.rs --lines 100:
```

Pipe the newest contributor SHA into another tool:

```sh
ohara explain src/auth.rs --lines 1:50 | jq -r '.hits[0].commit_sha'
```

## Notes

- Line numbers are 1-based and inclusive on both bounds. Open-ended
  ranges resolve `END = 0` to the file's actual last line by reading
  the workdir checkout.
- Every result has `provenance = "EXACT"` — `explain_change` is
  backed by `git blame`, not embeddings.
- The `_meta.explain.blame_coverage` field reports the fraction of
  queried lines attributed to a known commit. Less than `1.0` means
  some lines landed on a SHA older than the local watermark — re-run
  `ohara index` to backfill.
