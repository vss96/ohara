# `ohara status`

Print the current freshness of a repo's index: where the watermark
is, when it was last updated, and how many commits exist on `HEAD`
that the index hasn't seen yet.

## Usage

```
ohara status [PATH]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo. |

## Example

```sh
ohara status
```

Sample output:

```
repo: /Users/alex/code/my-service
id: 9f1a3b2c8d4e5f6a
last_indexed_commit: a1b2c3d4e5f6...
indexed_at: 2026-04-30T18:11:00Z
commits_behind_head: 0
compatibility: compatible
```

`commits_behind_head` is computed against the current `HEAD` — a
non-zero value means `ohara index --incremental` has work to do.
`<none>` for `last_indexed_commit` / `indexed_at` means the repo
hasn't been indexed yet; run `ohara index`.

### Compatibility line (v0.7+)

The `compatibility` line reports whether the index was built with the
same embedder / chunker / parser / semantic-text versions the current
binary expects (plan 13). One of:

| Value | Meaning | Suggested action |
|---|---|---|
| `compatible` | Every recorded component matches the binary. | None. |
| `query-compatible, refresh recommended (<reason>)` | A derived component (chunker, parser, semantic-text, reranker) bumped versions; KNN still works but the derived rows are stale. | `ohara index --force` |
| `needs rebuild (<reason>)` | A vector-affecting component (embedding model, dimension, schema) differs. KNN against this index would be wrong. | `ohara index --rebuild` |
| `unknown (no metadata for …)` | Pre-v0.7 index, or freshly-migrated repo before any v0.7+ pass wrote metadata. | `ohara index --force` records current versions. |

`status` does not load the embedder model, so the compatibility check
is fast and works on machines without the model cache populated.

## Use it from CI

`ohara status` is cheap (no embedder boot) and machine-readable enough
to grep:

```sh
behind=$(ohara status | awk '/commits_behind_head/ { print $2 }')
[ "$behind" = "0" ] || ohara index --incremental
```
