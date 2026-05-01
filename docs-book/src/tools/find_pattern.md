# `find_pattern`

Semantic search over a repo's git history. Answers **"how was X done
before?"** by ranking historical commits whose diffs resemble a
natural-language query.

Backed by the three-lane retrieval pipeline (vector KNN + FTS5 BM25
hunk-text + FTS5 BM25 symbol-name) → Reciprocal Rank Fusion →
cross-encoder rerank → recency tie-break. See the
[retrieval pipeline](../architecture/retrieval-pipeline.md) page for
the full architecture.

## When to use

**USE WHEN** the user:

- asks "how did we do X before?" / "is there a pattern for Y?"
- requests adding a feature similar to existing functionality
  ("add retry like we did before", "make this look like the auth
  flow")
- is about to write code that likely has prior art in this repo

**DO NOT USE** for searching current code — use Grep/Read for that.
**DO NOT USE** for general programming questions.

## Input parameters

Schema source: `FindPatternInput` in
`crates/ohara-mcp/src/tools/find_pattern.rs`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `query` | string | required | Natural-language description of the pattern to find. |
| `k` | integer | `5` | Number of results to return; clamped to `1..=20`. |
| `language` | string | `null` | Optional language filter (e.g. `"rust"`, `"python"`, `"java"`, `"kotlin"`). |
| `since` | string | `null` | Optional lower bound on commit age. Accepts ISO date (`"2024-01-01"`), RFC-3339 datetime, or relative days (`"30d"`). |
| `no_rerank` | boolean | `false` | Skip the cross-encoder rerank stage. Faster, deterministic, slightly less precise on the top result. |

## Output shape

The tool returns a single JSON document with `hits` and `_meta`. Each
hit follows the `PatternHit` shape from `ohara-core::query`:

```json
{
  "hits": [
    {
      "commit_sha": "a1b2c3d4...",
      "commit_message": "Add exponential backoff to HTTP client",
      "commit_author": "Alex Doe",
      "commit_date": "2024-09-12T14:23:00Z",
      "file_path": "src/http/retry.rs",
      "change_kind": "modified",
      "diff_excerpt": "@@ -10,3 +10,12 @@\n+    let mut delay = base_delay;\n+    for attempt in 0..max_attempts { ...",
      "diff_truncated": false,
      "related_head_symbols": ["http::retry::with_backoff"],
      "similarity": 0.83,
      "recency_weight": 0.94,
      "combined_score": 0.79,
      "provenance": "INFERRED"
    }
  ],
  "_meta": {
    "index_status": {
      "last_indexed_commit": "a1b2c3d4...",
      "commits_behind_head": 0,
      "indexed_at": "2026-04-30T18:11:00Z"
    },
    "hint": null
  }
}
```

`provenance` is always `"INFERRED"` — `find_pattern` is a semantic
match, not a deterministic lookup. (For deterministic results see
[`explain_change`](./explain_change.md), where provenance is always
`"EXACT"`.)

`_meta.hint` is populated when the index is empty, stale, or otherwise
unable to answer the query usefully — surface it to the user so they
know to run `ohara index`.

## Example

Invocation from an MCP client:

```json
{
  "name": "find_pattern",
  "arguments": {
    "query": "retry an HTTP request with exponential backoff",
    "k": 3,
    "language": "rust",
    "since": "180d"
  }
}
```

The same call from the CLI for debugging:

```sh
ohara query --query "retry an HTTP request with exponential backoff" --k 3 --language rust
```

Add `--no-rerank` (CLI) or `"no_rerank": true` (MCP) to skip the
cross-encoder stage when latency matters more than top-1 precision.
