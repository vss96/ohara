# ohara

<p align="center">
  <img src="docs/img/ohara-tree.png" alt="Ohara, with the Tree of Knowledge" width="640" />
</p>

Local-first context lineage engine. Indexes a git repo's commits and diffs, then
serves "how was X done before?" queries to Claude Code (or any MCP client) via a
local stdio server.

Named after Ohara, the island in One Piece whose Tree of Knowledge held 5,000
years of accumulated history — and whose archaeologists devoted their lives to
reading it.

This repo is at **Plan 1**: foundation + the `find_pattern` MCP tool. The
`explain_change` tool, git-hook installation, and additional language support
arrive in subsequent plans.

## Install

Pre-built binaries are published on each release. To install both binaries on
macOS or Linux:

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vss96/ohara/releases/latest/download/ohara-cli-installer.sh | sh
    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vss96/ohara/releases/latest/download/ohara-mcp-installer.sh | sh

On Windows (PowerShell):

    powershell -ExecutionPolicy Bypass -c "irm https://github.com/vss96/ohara/releases/latest/download/ohara-cli-installer.ps1 | iex"
    powershell -ExecutionPolicy Bypass -c "irm https://github.com/vss96/ohara/releases/latest/download/ohara-mcp-installer.ps1 | iex"

Or grab a tarball directly from the [releases page](https://github.com/vss96/ohara/releases).

## Build from source

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
      "command": "/absolute/path/to/ohara-mcp",
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
