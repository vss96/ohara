# ohara

Local-first context lineage engine. Indexes a git repo's commits and diffs, then
serves "how was X done before?" queries to Claude Code (or any MCP client) via a
local stdio server.

This repo is at **Plan 1**: foundation + the `find_pattern` MCP tool. The
`explain_change` tool, git-hook installation, and additional language support
arrive in subsequent plans.

## Build

    cargo build --release

Produces two binaries under `target/release/`:
- `ohara` — CLI for indexing and debugging
- `ohara-mcp` — MCP server (stdio) for Claude Code

## Quickstart

    fixtures/build_tiny.sh
    cargo run -p ohara-cli -- index fixtures/tiny/repo
    cargo run -p ohara-cli -- query --query "retry with backoff" fixtures/tiny/repo

The first run downloads the BGE-small embedding model (~80MB, one time).

## Wiring into Claude Code

In your `~/.claude/claude_desktop_config.json` (or per-repo MCP config), add:

```json
{
  "mcpServers": {
    "ohara": {
      "command": "/absolute/path/to/target/release/ohara-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

The server reads the current working directory of the spawning Claude Code
session as the repo to query. Run `ohara index` first.

## Layout

See `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md` for the
v1 design and `docs/superpowers/plans/` for implementation plans.
