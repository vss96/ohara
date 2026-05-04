# ohara

<p align="center">
  <img src="./img/ohara-tree.webp" alt="Ohara, with the Tree of Knowledge" width="640" />
</p>

ohara is a **local-first context lineage engine** that indexes a git repo's
commits, diffs, and source symbols and serves them to AI coding assistants
through the Model Context Protocol (MCP).

Where Grep answers "where is this code?", ohara answers two complementary
questions about the *history* of the code:

- **`find_pattern`** — "how was X done before?" Returns historical commits
  whose diffs resemble a natural-language query, ranked by a three-lane
  retrieval pipeline (vector + BM25 hunk-text + BM25 symbol-name) →
  Reciprocal Rank Fusion → cross-encoder rerank → recency tie-break.
- **`explain_change`** — "why does THIS code look the way it does?"
  Given a file + line range, returns the commits that introduced and
  shaped those lines, ordered newest-first. Deterministic — backed by
  `git blame`, not embeddings.

## Design principles

- **Local-first.** All indexing, embedding, and retrieval happens on your
  machine. No cloud calls. The SQLite-based index lives under
  `$OHARA_HOME/<repo-id>/index.sqlite`.
- **Stays out of the way.** A post-commit hook (installed by
  `ohara init`) keeps the index fresh; the `--incremental` fast path
  makes that essentially free.
- **Idempotent and abort-safe.** Killed mid-index? Resume re-does at
  most ~100 commits.
- **Single static binary by default.** By default ohara is local-first single-process;
  an opt-in `ohara serve` daemon is available for warm-cache workflows.
  Indexing is still always foreground. Distributed via `cargo-dist` for macOS
  and Linux (Windows users: WSL).

## Where to go next

- [Install](./install.md) — `curl | sh` install or build from source.
- [Quickstart](./quickstart.md) — index a repo and run your first query.
- [Wiring into MCP clients](./mcp-clients.md) — point Claude Code,
  Cursor, Codex, OpenCode, or any MCP-aware client at the server.
- [Architecture overview](./architecture/overview.md) — for contributors
  and the curious.

## Status

Released versions: v0.1 (foundation + `find_pattern`) → v0.2 (auto-freshness)
→ v0.3 (retrieval pipeline upgrade) → v0.4 (Java + Kotlin support) →
v0.5 (`explain_change` tool) → v0.5.1 (progress bar + abort-resume +
self-update) → v0.6 (indexing throughput prep) → v0.7.0–v0.7.5 (evals,
perf tracing, memory-efficient indexing, `ohara serve` daemon + multi-repo).
**Current: v0.7.5.** See the [Roadmap](./roadmap.md).
