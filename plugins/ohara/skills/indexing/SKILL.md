---
description: Manage the ohara index lifecycle. Use when the user asks to index a repo, when an MCP response reports a compatibility issue (needs_rebuild / refresh recommended), or when explaining the difference between --incremental, --force, and --rebuild.
---

# Ohara Indexing

Ohara stores a per-repo SQLite index at `$OHARA_HOME/<repo-id>/db.sqlite`
(default `~/.ohara`). Maintaining this index is what makes `find_pattern` and
`explain_change` work.

## First-time setup

```bash
ohara index <repo-path>
```

Downloads BGE-small (~80 MB) on first run into `.fastembed_cache/`. Walks every
commit, parses changed files (Rust / Python / Java / Kotlin), embeds hunks,
writes vector + FTS5 rows. Resume-safe: a crash mid-pass replays cleanly.

## Day-to-day: incremental

```bash
ohara index --incremental <repo-path>
```

Walks only commits newer than the last watermark. No-op if at HEAD (does not
even load the embedder). Use after `git pull`.

## When `ohara status` says "refresh recommended"

A *derived* component (chunker version, parser version, semantic-text version,
reranker model) bumped. KNN vectors are still valid — re-run derived work in
place:

```bash
ohara index --force <repo-path>
```

`--force` wins over `--incremental` if both are set.

## When `ohara status` says "needs rebuild"

The embedder model or vector dimension changed. Stored vectors are now wrong
against any new query embedding. The only safe answer is to drop and rebuild:

```bash
ohara index --rebuild --yes <repo-path>
```

`--yes` is required (refuses without it). Conflicts with `--incremental` and
`--force` at the clap layer. Slow — re-walks every commit and re-embeds every
hunk.

## Compatibility verdicts (full table)

| Verdict | Trigger | Recovery |
|---|---|---|
| `compatible` | Every recorded component matches the binary | none |
| `query_compatible_needs_refresh` | Derived component bumped (chunker / parser / semantic-text / reranker) | `ohara index --force` |
| `needs_rebuild` | Vector-affecting component changed (embedder model, dimension) | `ohara index --rebuild --yes` |
| `unknown` | Pre-v0.7 index, no metadata rows yet | `ohara index --force` populates them |

## What to surface to the user

If a `find_pattern` call returns an error mentioning `needs rebuild`, do not
retry. Tell the user the exact command (`ohara index --rebuild --yes
<repo-path>`) and why (vector dimension or embedder model differs from what
the index was built with). `explain_change` continues to work under every
verdict because blame doesn't touch vectors.

## Useful flags

- `--profile` — emits `PhaseTimings` JSON to stdout after the run summary.
  Useful for diagnosing slow indexing. Pipe to `jq`.
- `--commit-batch <N>` — commits per storage transaction. Smaller = less RAM,
  more fsyncs. Larger = faster, more memory.
- `--threads <N>` — parser/embedder thread count. `0` lets ohara pick.
- `--embed-provider {auto,cpu,coreml,cuda}` — ONNX execution provider. CoreML
  and CUDA require building with the matching cargo feature.

## Reference

Full architecture doc:
[`docs-book/src/architecture/indexing.md`](https://github.com/vss96/ohara/blob/main/docs-book/src/architecture/indexing.md).
