# Wiring into MCP clients

ohara ships as a stdio MCP server (`ohara-mcp`). Any MCP-aware client
can talk to it. The server exposes two tools — `find_pattern` and
`explain_change` — backed entirely by the local SQLite index. No
network calls, no shared state.

## The shape every client uses

The Model Context Protocol standardizes server registration as a
small JSON object. Most clients accept this exact shape (some
re-spell it slightly):

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

Replace the path with `which ohara-mcp` after install. The server
reads the **current working directory of the spawning client session**
as the repo to query — open the client in an indexed repo and ohara's
tools become available for that repo automatically.

## Client-by-client config locations

The exact file path is the only thing that varies. Always check your
client's docs for the canonical answer; the entries below reflect
what was current at the time of writing.

### Claude Code / Claude Desktop

- **Global:** `~/.claude/claude_desktop_config.json`
- **Per-repo:** `.mcp.json` or `.claude/mcp.json` in the repo root
- **CLI shortcut:** `claude mcp add ohara /absolute/path/to/ohara-mcp`
- Restart Claude after editing the config.

### Cursor

- **Global:** `~/.cursor/mcp.json`
- **Per-workspace:** `.cursor/mcp.json` in the project root
- Same JSON shape as above. Cursor discovers MCP servers on startup;
  reload the workspace after a config change.

### OpenAI Codex CLI

Codex CLI uses TOML rather than JSON. Add to `~/.codex/config.toml`:

```toml
[mcp_servers.ohara]
command = "/absolute/path/to/ohara-mcp"
args = []
```

The block name (`ohara`) is the server identifier the model sees in
tool selection — keep it short and descriptive.

### OpenCode

OpenCode reads either `~/.config/opencode/opencode.json` (global) or
an `opencode.json` in the workspace root (per-project). The MCP block
nests under an `mcp` key:

```json
{
  "mcp": {
    "ohara": {
      "type": "local",
      "command": ["/absolute/path/to/ohara-mcp"]
    }
  }
}
```

Note `command` is an array (the binary plus any args).

### Other / generic MCP clients

Any client that follows the MCP spec accepts the standard JSON shape
at the top of this page. If your client supports per-server `env`
overrides, the only env var ohara reads is `OHARA_HOME` (the index
location, defaults to `~/.ohara`).

## Per-repo wiring (recommended for teams)

When teammates have `ohara-mcp` at different paths, commit a
`.mcp.json` (or `.cursor/mcp.json`, etc.) to the repo with a relative
path or a portable command. Per-repo configs override global ones in
every client tested above.

## What every client gets

Two tools, both deterministic-ish, both read-only against the index:

- [`find_pattern`](./tools/find_pattern.md) — semantic search over
  git history. Use when the user asks "how have we done X before?",
  "is there a pattern for Y?", or is about to write code that has
  prior art in this repo.
- [`explain_change`](./tools/explain_change.md) — git-blame-backed
  archaeology. Use when the user asks "why does this code look this
  way?" or wants the commits that shaped a specific file + line
  range.

The MCP server's `instructions` field tells the model when to reach
for ohara vs. generic search. Most clients propagate it through tool
selection automatically.

## Bootstrapping

Two prerequisites for the MCP server to return useful results:

1. **The repo is indexed.** Run `ohara index` once. Both tools
   degrade gracefully on an empty index (zero hits + a `_meta.hint`
   explaining why) but they're not interesting until there's history
   to look at.
2. **The index stays fresh.** Run `ohara init` to install the
   post-commit hook. After that, every commit triggers an
   `--incremental` re-index — typically sub-second.

Both steps are described in detail in the [Quickstart](./quickstart.md).

## Verifying the wiring

In a session inside an indexed repo, ask the model:

> Use `find_pattern` to look up "retry with backoff" in this repo.

If the wiring is good, the model invokes the tool and shows the JSON
hits. If it isn't, the tool simply won't appear in the tool list.
Common causes:

- Wrong absolute path — `which ohara-mcp` and recheck.
- Binary not executable — `chmod +x` if you unpacked a tarball by hand.
- Client wasn't restarted / workspace wasn't reloaded after the config edit.
- Working directory on launch doesn't match a repo that's been indexed.
