# ohara — Claude Code plugin

Claude Code plugin for [ohara](https://github.com/vss96/ohara), a local-first
git-history lineage engine. Adds the `ohara` MCP server (`find_pattern` +
`explain_change`) plus two skills:

- **`ohara:lineage`** — when to reach for `find_pattern` / `explain_change` vs
  `Grep`. Triggers on "how did we do X before?" / "why does this code look this
  way?" questions.
- **`ohara:indexing`** — index lifecycle. When to run `ohara index`, the
  difference between `--incremental` / `--force` / `--rebuild`, and how to
  recover from a `needs_rebuild` verdict.

## Install

```text
/plugin marketplace add vss96/ohara
/plugin install ohara@vss96
```

The plugin's MCP entry registers a stdio server under the name `ohara`. On
first use, `bin/ohara-mcp` (a small Node wrapper) downloads the matching
`ohara-mcp` release binary for your platform from
`https://github.com/vss96/ohara/releases` and caches it at
`~/.cache/ohara-plugin/v<version>/`. Subsequent starts skip the download.

### Index a repo

The plugin ships only the MCP server side. To index a repo, install the
`ohara` CLI separately (see the
[main README](https://github.com/vss96/ohara#readme)) and run:

```sh
ohara index <repo-path>
```

The MCP server queries whatever index exists for the spawning client's CWD.

## Requirements

- Node.js 18+ (for the `bin/ohara-mcp` wrapper)
- `tar` with xz support on `PATH` (default on macOS and Linux)
- An ohara-indexed repo (`ohara index <repo-path>` from the CLI)

## Supported platforms

- macOS aarch64 (Apple Silicon)
- macOS x86_64 (Intel)
- Linux x86_64
- Linux aarch64

Windows is not shipped as a release binary (ort_sys link issue, see the
ohara `dist-workspace.toml`). WSL users can use the Linux x86_64 binary.
Other targets must build `ohara-mcp` from source and put it on `PATH`.

## Configuration

| Env var | Effect |
|---|---|
| `OHARA_PLUGIN_VERSION` | Override the release tag the wrapper downloads. Defaults to the plugin version. |
| `OHARA_HOME` | Where ohara stores per-repo indexes. Defaults to `~/.ohara`. |

## Layout

```text
plugins/ohara/
├── .claude-plugin/plugin.json
├── .mcp.json                  # registers `ohara` stdio MCP server
├── bin/ohara-mcp              # Node wrapper, downloads + execs the binary
├── skills/
│   ├── lineage/SKILL.md
│   └── indexing/SKILL.md
├── package.json
└── README.md
```

The marketplace catalog lives at `.claude-plugin/marketplace.json` in the
ohara repo root.

## Versioning

The plugin version tracks the ohara release. When ohara ships v0.7.5, bump
`plugins/ohara/.claude-plugin/plugin.json` and `package.json` to match — the
wrapper will then fetch `v0.7.5` artifacts.
