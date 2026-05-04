---
description: Use the ohara MCP (find_pattern + explain_change) to answer "how did we do X before?" or "why does this code look this way?" against this repo's git history. Trigger on lineage / prior-art / git-archaeology questions; skip for current-state grep, fresh repos with no history, or general programming questions.
---

# Ohara Lineage

This repository has been indexed by [ohara](https://github.com/vss96/ohara), a
local-first git-history lineage engine. Two MCP tools are available on the
`ohara` server.

## Use `find_pattern` when

- The user asks "how did we do X before?" / "is there a pattern for Y?"
- The user wants to add a feature similar to existing functionality
  ("retry like we did before", "make this look like the auth flow")
- You are about to write code that likely has prior art in this repo

## Use `explain_change` when

- The user asks "why does this code look this way?" / "how did this get here?"
- The user wants git archaeology / blame for a function or specific line range
- You need the history of a block, function, or `(file, start, end)` range

## Do NOT use these tools for

- Searching current code state — use `Grep` / `Read` instead
- Confirming a feature exists today (these surface history, not present state —
  a feature can be planned-but-unbuilt and still rank highly)
- Brand-new files or fresh repos with no commit history
- General programming / framework questions that don't depend on this repo

## Reading the response

Both tools return a `_meta.compatibility` field describing the index state:

- `compatible` — proceed normally
- `query_compatible_needs_refresh` — queries still work; suggest
  `ohara index --force` for completeness
- `needs_rebuild` — `find_pattern` will refuse with a structured error.
  Surface the rebuild command (`ohara index --rebuild --yes`) to the user.
  Do not silently retry.
- `unknown` — pre-v0.7 index without metadata; suggest `ohara index --force`

For index lifecycle questions (when to run, `--force` vs `--rebuild`, etc.) see
the `ohara:indexing` skill.

## Quality signals to watch

- **All `combined_score` values negative** → low confidence. Treat as
  "no strong prior art" rather than acting on the top hit.
- **`provenance: "INFERRED"`** is normal for `find_pattern` (semantic match);
  `explain_change` returns `"EXACT"` because git blame is git-truth.
- **`recency_weight` near 1.0** means the hit is recent; near 0.0 means old.
  The cross-encoder rerank already factors this in — don't double-discount.
