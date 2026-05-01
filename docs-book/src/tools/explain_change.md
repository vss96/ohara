# `explain_change`

Git-archaeology over a specific file and line range. Answers **"why
does THIS code look the way it does?"** by walking `git blame` and
returning the commits that introduced and shaped those lines,
newest-first.

Deterministic — backed by `git blame`, not embeddings. Every result
has `provenance = "EXACT"`. Companion to
[`find_pattern`](./find_pattern.md), which is semantic.

## When to use

**USE WHEN** the user:

- asks "why does this code look this way?" / "how did this get here?"
- wants "git archaeology" / "who wrote this?" / "blame this"
- wants the history of a specific block, function, or line range

**DO NOT USE** for:

- searching for similar past patterns — use
  [`find_pattern`](./find_pattern.md) instead
- inspecting current code — use Grep/Read for that
- general programming questions

## Input parameters

Schema source: `ExplainChangeInput` in
`crates/ohara-mcp/src/tools/explain_change.rs`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `file` | string | required | Repo-relative file path (e.g. `src/auth.rs`). |
| `line_start` | integer | `1` | 1-based start line, inclusive. |
| `line_end` | integer | `0` | 1-based end line, inclusive. `0` is a sentinel meaning "end of file" — the server resolves it by reading the workdir checkout. |
| `k` | integer | `5` | Number of commits to return; clamped to `1..=20`. |
| `include_diff` | boolean | `true` | Include `diff_excerpt` in each hit. Set to `false` for a tighter response when only the blame attribution matters. |

If only `file` is provided, the tool explains the whole file (line 1
through end-of-file) with the default `k = 5`.

## Output shape

A JSON document with `hits` and `_meta`. Each hit follows the
`ExplainHit` shape from `ohara-core::explain`:

```json
{
  "hits": [
    {
      "commit_sha": "9f8e7d6c...",
      "commit_message": "Refactor auth: extract token validator",
      "commit_author": "Alex Doe",
      "commit_date": "2024-11-03T09:42:00Z",
      "blame_lines": [42, 43, 44, 45],
      "file_path": "src/auth.rs",
      "diff_excerpt": "@@ -38,6 +38,12 @@ ...",
      "diff_truncated": false,
      "provenance": "EXACT"
    }
  ],
  "_meta": {
    "index_status": { "last_indexed_commit": "9f8e7d6c...", "commits_behind_head": 0, "indexed_at": "2026-04-30T18:11:00Z" },
    "hint": null,
    "explain": {
      "lines_queried": [40, 60],
      "commits_unique": 1,
      "blame_coverage": 1.0,
      "limitation": null
    }
  }
}
```

Notes on the `_meta.explain` block:

- `lines_queried` reflects the **clamped** range (the server clamps
  `line_end` to the file's actual length).
- `blame_coverage` is the fraction of queried lines that resolved to
  a commit the local index knows about. Less than `1.0` means at
  least one line landed on a SHA older than the current watermark
  (re-run `ohara index` to backfill).
- `limitation` is a free-form note when the result set is constrained
  (e.g. "file does not exist in HEAD" or "file was renamed; pre-rename
  history not reached").

## Example

Invocation from an MCP client:

```json
{
  "name": "explain_change",
  "arguments": {
    "file": "src/auth.rs",
    "line_start": 40,
    "line_end": 60,
    "k": 3,
    "include_diff": true
  }
}
```

The same call from the CLI for debugging:

```sh
ohara explain src/auth.rs --lines 40:60 --k 3
```

Open-ended ranges work too: `--lines :42` (start to line 42),
`--lines 10:` (line 10 to end-of-file), no `--lines` at all (the
whole file).
