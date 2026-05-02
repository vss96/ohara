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

**Status: v0.6.** Two MCP tools shipped, plus throughput-prep
plumbing and opt-in hardware acceleration on the indexer:

- **`find_pattern`** — "how was X done before?" (semantic search over git
  history with three-lane retrieval pipeline + cross-encoder rerank,
  shipped in v0.3).
- **`explain_change`** — "why does THIS code look the way it does?"
  (deterministic git-blame-based commit lookup for a file + line
  range; new in v0.5).

v0.6 highlights: `--profile` per-phase wall-time JSON for the
throughput baseline; `--embed-provider {auto,cpu,coreml,cuda}`
auto-detect + `--resources {auto,conservative,aggressive}` policy;
resume-crash fix in `commit::put` (DELETE-then-INSERT for
`vec_commit` / `fts_commit`); a pinned progress bar that no longer
scrolls off-screen when `tracing` log lines stream above it.

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
