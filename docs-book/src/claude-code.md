# Wiring into Claude Code

ohara ships as a stdio MCP server (`ohara-mcp`). Any MCP-aware client
can talk to it — Claude Code, Claude Desktop, Cursor, Zed, custom
clients. Configuration is the same shape everywhere: tell the client
the absolute path to the binary.

## Claude Desktop / Claude Code config

Open (or create) `~/.claude/claude_desktop_config.json` and add an
entry under `mcpServers`:

```json
{
  "mcpServers": {
    "ohara": {
      "command": "/absolute/path/to/ohara-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

Replace `/absolute/path/to/ohara-mcp` with the result of
`which ohara-mcp` after install. Restart Claude after editing the
config.

The server reads the **current working directory of the spawning
Claude session** as the repo to query. This means:

- Open Claude Code in a repo that's been indexed by `ohara index`,
  and ohara's tools become available for that repo automatically.
- Switch repos and the next session's tools are scoped to that
  repo's index.

## Per-repo MCP config

Some clients honor a per-repo `.mcp.json` or `.claude/mcp.json`. The
shape is identical to the global `mcpServers` block above. Useful when
different teammates have `ohara-mcp` at different absolute paths but
want the same logical wiring committed to the repo.

## What Claude gets

Two tools, both backed by the local SQLite index — no network calls:

- [`find_pattern`](./tools/find_pattern.md) — semantic search over
  git history. Use when the user asks "how have we done X before?",
  "is there a pattern for Y?", or is about to write code that has
  prior art in this repo.
- [`explain_change`](./tools/explain_change.md) — git-blame-backed
  archaeology. Use when the user asks "why does this code look this
  way?" or wants the commits that shaped a specific file + line
  range.

The server's `instructions` field tells the model when to reach for
ohara vs. generic search — Claude Code picks this up automatically
during tool selection.

## Bootstrapping

Two prerequisites for the MCP server to return useful results:

1. **The repo is indexed.** Run `ohara index` once. Both tools
   degrade gracefully on an empty index (they return zero hits with
   a `_meta.hint` explaining why) but they're not interesting until
   there's history to look at.
2. **The index stays fresh.** Run `ohara init` to install the
   post-commit hook. After that, every commit triggers an
   `--incremental` re-index — typically sub-second.

Both steps are described in detail in the [Quickstart](./quickstart.md).

## Verifying the wiring

In a Claude Code session inside an indexed repo, ask:

> Use `find_pattern` to look up "retry with backoff" in this repo.

If the wiring is good, Claude calls the tool and shows the JSON hits.
If it isn't, the tool simply won't appear in Claude's tool list —
check the path in `claude_desktop_config.json`, restart Claude, and
make sure `ohara-mcp` is executable (`chmod +x` if you unpacked a
tarball by hand).
