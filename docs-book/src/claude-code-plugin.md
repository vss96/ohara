# Claude Code plugin

The recommended way to use ohara from [Claude Code](https://docs.claude.com/en/docs/claude-code)
is the official plugin. It registers the `ohara-mcp` server, ships two
skills that teach the model when to reach for lineage queries, and
auto-downloads the binary on first use.

For other MCP clients (Cursor, Codex CLI, OpenCode, etc.) see
[Wiring into MCP clients](./mcp-clients.md).

## Install

```text
/plugin marketplace add vss96/ohara
/plugin install ohara@vss96
/reload-plugins
```

That's it. The plugin's wrapper (`bin/ohara-mcp`) downloads the
matching `ohara-mcp` release tarball for your platform on first
invocation and caches it at `~/.cache/ohara-plugin/v<version>/`.
Subsequent starts skip the download.

## What the plugin ships

| Component | Purpose |
|---|---|
| `ohara` MCP server | Registers `find_pattern` and `explain_change` as MCP tools. |
| `ohara:lineage` skill | Tells Claude when to use `find_pattern` / `explain_change` vs `Grep` — triggers on "how did we do X before?", "why does this code look this way?", and prior-art questions. |
| `ohara:indexing` skill | Index lifecycle guidance — `--incremental` vs `--force` vs `--rebuild`, and how to recover from each [compatibility verdict](./architecture/indexing.md#index-compatibility-v07). |

## Prerequisites

- **Node.js 18+** — the wrapper that downloads the binary is a small
  Node script. macOS and most Linux distros ship Node 18+ by default.
- **`tar` with xz support** — default on macOS and Linux. Used to
  extract the release tarball.
- **An indexed repo.** The plugin only ships the MCP server; you index
  with the `ohara` CLI separately:

  ```sh
  ohara index <repo-path>
  ```

## Supported platforms

The plugin's binary download matches the ohara release matrix:

- macOS aarch64 (Apple Silicon)
- macOS x86_64 (Intel)
- Linux x86_64
- Linux aarch64

Windows is not shipped as a release binary (`ort_sys` link issue;
WSL users can use the Linux x86_64 binary). Other targets need to
build `ohara-mcp` from source and put it on `PATH` — but at that
point you don't need the plugin's downloader; manual MCP wiring works
just as well.

## Updating

When ohara cuts a new release, refresh the marketplace and reinstall:

```text
/plugin marketplace update vss96
/plugin install ohara@vss96
```

The wrapper's hard-coded version determines which release tarball
gets fetched, so plugin updates and ohara releases stay in lock-step.

## Uninstall

```text
/plugin uninstall ohara@vss96
/plugin marketplace remove vss96
```

The cached binary at `~/.cache/ohara-plugin/` is not removed
automatically — `rm -rf ~/.cache/ohara-plugin` to clean up.

## Troubleshooting

### "ohara MCP server didn't start"

Most likely the binary download failed silently. Run the wrapper
manually to see the download log:

```sh
~/.claude/plugins/marketplaces/vss96/plugins/ohara/bin/ohara-mcp <<< ''
```

Common causes:
- No network access on the MCP host
- Firewall blocking `https://github.com/vss96/ohara/releases/...`
- Cached binary corrupted — `rm -rf ~/.cache/ohara-plugin/`

### "find_pattern returns errors mentioning rebuild"

The index was built with an embedder that doesn't match the binary
the plugin just downloaded. Run:

```sh
ohara index --rebuild --yes <repo-path>
```

See [index compatibility](./architecture/indexing.md#index-compatibility-v07)
for the full verdict table — the `ohara:indexing` skill teaches Claude
to surface this command when it happens.

### "Plugin loaded but skills don't trigger"

`/reload-plugins` after install. Skills are namespaced as
`/ohara:lineage` and `/ohara:indexing` — `/help` should list them.

## What's in the repo

The plugin lives inside the ohara repository at `plugins/ohara/`:

```text
plugins/ohara/
├── .claude-plugin/plugin.json
├── .mcp.json                 # registers the ohara stdio MCP server
├── bin/ohara-mcp             # Node wrapper that fetches the binary
├── skills/
│   ├── lineage/SKILL.md
│   └── indexing/SKILL.md
├── package.json
└── README.md
```

The marketplace catalog at `.claude-plugin/marketplace.json` makes the
ohara repo itself act as a single-plugin marketplace.
