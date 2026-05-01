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

**Status: v0.4.** v0.1 shipped Plan 1 (foundation + the `find_pattern`
MCP tool); v0.2 shipped Plan 2 (`ohara init` post-commit hook + `ohara
index --incremental` fast path); v0.3 shipped Plan 3 (three-lane
retrieval pipeline: vector KNN + FTS5 BM25 on hunk text + FTS5 BM25 on
symbol names → Reciprocal Rank Fusion → cross-encoder rerank
(`bge-reranker-base`) → recency multiplier; AST sibling-merge chunking).
v0.4 (this release) adds Java 17/21+ and Kotlin 1.9/2.0+ language
support: sealed types, records, data classes, objects, companions, and
annotations on declarations are all indexed. The `explain_change` tool
arrives in v0.5.

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
session as the repo to query. Run `ohara index <repo>` once to bootstrap, then
keep the index fresh with the post-commit hook:

    ohara init <repo>                   # installs .git/hooks/post-commit
    ohara init <repo> --write-claude-md # also appends an "ohara" stanza to CLAUDE.md

The hook runs `ohara index --incremental` after every commit. It's safe to
re-run `ohara init` (idempotent) and the hook fails closed if the `ohara`
binary isn't on `PATH` (won't block your commits).

## Layout

See `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md` for the
v1 design and `docs/superpowers/plans/` for implementation plans.
