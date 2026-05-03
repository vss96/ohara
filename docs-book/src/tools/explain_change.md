# `explain_change`

Git-archaeology over a specific file and line range. Two questions
in one tool:

1. **"Which commits introduced these lines?"** Backed by `git blame`,
   exact attribution. Each blame `hits[i]` carries
   `provenance = "EXACT"`.
2. **"What nearby changes shaped this area?"** Plan 12 enrichment.
   Contextual commits that touched the same file around the blame
   anchors, returned under `_meta.explain.related_commits` with
   `provenance = "INFERRED"` so clients don't confuse them with
   line-level proof.

Deterministic — backed by `git blame` + a cheap indexed
`get_neighboring_file_commits` lookup, not embeddings. Companion to
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
| `include_related` | boolean | CLI: `true` / MCP: `false` | Plan 12 Task 3.2 — attach contextual commits under `_meta.explain.related_commits`. CLI defaults to on so `ohara explain` answers include nearby context; MCP defaults to off to keep the response payload predictable. |

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
      "limitation": null,
      "related_commits": [
        {
          "commit_sha": "5a4b3c2d...",
          "commit_message": "auth: add token refresh helper",
          "commit_author": "Bob",
          "commit_date": "2024-10-20T14:11:00Z",
          "touched_hunks": 2,
          "provenance": "INFERRED"
        }
      ]
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
- `related_commits` (plan 12) is the file-scope context list — commits
  that touched the same file near each blame anchor. **These are NOT
  proof of line-level ownership**; they're labelled
  `provenance = "INFERRED"` and capped at 2 commits before + 2
  commits after each blame anchor, deduped across anchors. The list
  is omitted entirely when `include_related = false` or no
  neighbouring commits exist.
- `enrichment_limitation` is a free-form note when enrichment was
  constrained (e.g. "no indexed blame anchors — no contextual
  neighbours available").

### How to read the two response sections

| Field | Provenance | Meaning |
|---|---|---|
| `hits[i]` | `EXACT` | This commit introduced these specific lines (git blame). |
| `_meta.explain.related_commits[i]` | `INFERRED` | This commit touched the same file in the same time window, but doesn't necessarily own these lines. Use as context, not proof. |

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
