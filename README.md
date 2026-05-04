# ohara

<p align="center">
  <img src="docs/img/ohara-tree.webp" alt="Ohara, with the Tree of Knowledge" width="640" />
</p>

Local-first context lineage engine. Indexes a git repo's commits and diffs, then
serves "how was X done before?" queries to Claude Code (or any MCP client) via a
local stdio server.

Named after Ohara, the island in One Piece whose Tree of Knowledge held 5,000
years of accumulated history — and whose archaeologists devoted their lives to
reading it.

**Status: v0.7.5.** Two MCP tools shipped (`find_pattern`, `explain_change`),
plus an opt-in daemon, multi-repo support, and memory-efficient indexing:

- **`find_pattern`** — "how was X done before?" (semantic search over git
  history with three-lane retrieval pipeline + cross-encoder rerank,
  shipped in v0.3).
- **`explain_change`** — "why does THIS code look the way it does?"
  Blame-backed exact provenance (`hits[]`) plus contextual commit
  neighbours (`_meta.explain.related_commits[]`).

v0.7.x highlights:

- **v0.7.0** — eval harness + symbol attribution + rebuild safety (plans 10/11/13).
- **v0.7.2** — perf tracing + per-method storage metrics (plan-14).
- **v0.7.3** — memory-efficient indexing: `embed_batch` chunking + source-text cap + peak-RSS harness (plan-15).
- **v0.7.4** — gitlink-skip fix in `file_at_commit` for uninitialized submodules.
- **v0.7.5** — `ohara serve` daemon + `RetrievalEngine` + multi-repo support (plan-16).

History: v0.1 = Plan 1 foundation + `find_pattern`; v0.2 = `ohara init`
post-commit hook + `--incremental` fast path; v0.3 = three-lane
retrieval (vector KNN + FTS5 BM25 hunk-text + FTS5 BM25 symbol-name) →
RRF → cross-encoder rerank (`bge-reranker-base`) → recency multiplier
+ AST sibling-merge chunking; v0.4 = Java 17/21 and Kotlin 1.9/2.0
language support (sealed types, records, data classes, annotations
preserved in `source_text` for Spring-friendly retrieval); v0.5 =
`explain_change`; v0.5.1 = progress bar, abort-resume hardening, and
`ohara update`. The full per-release breakdown lives in the
[changelog](https://vss96.github.io/ohara/changelog.html).

Languages: **Rust, Python, Java, Kotlin.** Class- and method-level
annotations (`@RestController`, `@Service`, `@Component`,
`@SpringBootApplication`, etc.) stay inside `source_text`, so
embeddings and BM25 pick up Spring-style markers without any new query
syntax.

## Install

Pre-built binaries for macOS and Linux are published on each release:

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vss96/ohara/releases/latest/download/ohara-cli-installer.sh | sh
    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vss96/ohara/releases/latest/download/ohara-mcp-installer.sh | sh

Or grab a tarball directly from the [releases page](https://github.com/vss96/ohara/releases). Windows isn't supported yet (use WSL); see the [release notes](https://github.com/vss96/ohara/releases) for the current matrix.

### Updating

Self-update from the CLI:

    ohara update              # install the latest release in place
    ohara update --check      # just report whether a newer version exists

The cargo-dist installer also drops a standalone `ohara-cli-update`
script alongside the binary; either works.

## Build from source

    cargo build --release

Produces two binaries under `target/release/`:
- `ohara` — CLI for indexing and debugging
- `ohara-mcp` — MCP server (stdio) for Claude Code

### Build with hardware acceleration

The pre-built cargo-dist binaries are CPU-only — same artifact for
every host. To get hardware ONNX execution providers wired into the
embedder, build from source with the matching cargo feature:

    # Apple silicon — CoreML
    cargo build --release --features coreml

    # Linux x86_64 + NVIDIA — CUDA
    cargo build --release --features cuda

Pair the resulting binary with `ohara index --embed-provider coreml`
(or `cuda`); see [`ohara index`](https://vss96.github.io/ohara/cli/index.html)
for the full flag set. Default features stay CPU-only so the released
binaries work everywhere out of the box.

## Quickstart

    fixtures/build_tiny.sh
    cargo run -p ohara-cli -- index fixtures/tiny/repo
    cargo run -p ohara-cli -- query --query "retry with backoff" fixtures/tiny/repo

The first run downloads the BGE-small embedding model (~80MB, one time).
`ohara query` will auto-spawn a background daemon on first use to keep the
embedder warm; pass `--no-daemon` to skip it, or see `ohara daemon --help`.

## Wiring into MCP clients

### Claude Code: install the plugin

```text
/plugin marketplace add vss96/ohara
/plugin install ohara@vss96
/reload-plugins
```

Registers the MCP server, ships two skills (`ohara:lineage`,
`ohara:indexing`), and auto-downloads the binary on first use. Full
details in the [docs site](https://vss96.github.io/ohara/claude-code-plugin.html).

### Other MCP clients

`ohara-mcp` speaks stdio MCP, so any MCP-aware client picks it up
with the same shape:

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

Drop that block into the right config file:

- **Claude Code / Claude Desktop (manual fallback):** `~/.claude/claude_desktop_config.json`, `.mcp.json` per-repo, or `claude mcp add ohara <path>`. Prefer the plugin above.
- **Cursor:** `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (per-workspace).
- **OpenAI Codex CLI:** `~/.codex/config.toml` with a `[mcp_servers.ohara]` block (TOML, not JSON).
- **OpenCode:** `~/.config/opencode/opencode.json` or repo-root `opencode.json` under an `mcp` key.

Full config examples for each client live in the [docs site](https://vss96.github.io/ohara/mcp-clients.html).

The server reads the current working directory of the spawning client
session as the repo to query. Run `ohara index <repo>` once to bootstrap, then
keep the index fresh with the post-commit hook:

    ohara init <repo>                   # installs .git/hooks/post-commit
    ohara init <repo> --write-claude-md # also appends an "ohara" stanza to CLAUDE.md

The hook runs `ohara index --incremental` after every commit. It's safe to
re-run `ohara init` (idempotent) and the hook fails closed if the `ohara`
binary isn't on `PATH` (won't block your commits).

## Upgrading & index compatibility

Each ohara release records the embedder, chunker, and parser versions it
used to build the index. After upgrading, run `ohara status` — the
`compatibility:` line tells you whether the existing index is still
usable as-is, needs a cheap refresh, or needs a full rebuild. The two
recovery commands:

- `ohara index --force` — refreshes derived symbol/chunker rows when
  only those bumped. Commit + hunk + vector history is untouched.
- `ohara index --rebuild --yes` — drops the entire index and rebuilds
  from scratch. Required when the embedder model or vector dimension
  changed; KNN against a stale-vector index would otherwise return
  wrong results, and `find_pattern` (MCP) refuses to run until you
  rebuild.

Full design + the per-verdict table live in
[`docs-book/src/architecture/indexing.md`](https://vss96.github.io/ohara/architecture/indexing.html#index-compatibility-v07).

## Layout

See `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md` for the
v1 design and `docs/superpowers/plans/` for implementation plans.
