# ohara plan-16 — serve daemon

> **Status:** complete (commits e4df5f7..00603e1 on `feat/plan-16-serve-daemon`).

## Goal

Extract `ohara-mcp::OharaServer` into a standalone `ohara-engine` library crate
that both the MCP server and a new `ohara serve` daemon process can share. The
daemon holds embedding models and per-repo SQLite handles in memory, receives
IPC requests over a Unix socket, and keeps the hot-path sub-100ms for repeat
queries.

## Crates introduced

- `crates/ohara-engine` — `RetrievalEngine`, `RepoHandle`, `EmbeddingCache`,
  `MetaCache`, `BlameCache`, Unix-socket `Server` + `Client`, file-locked
  `Registry`, `spawn_daemon`.

## CLI additions

- `ohara serve` — starts the daemon process (daemonizes via `setsid`).
- `ohara daemon status|stop|list` — inspect or stop running daemons.
- `ohara query` and `ohara explain` transparently route through the daemon when
  one is available, falling back to in-process if not.
- `ohara index` sends an `Invalidate` message to all live daemons on success.

## Plan phases (A–I)

| Phase | Description |
|---|---|
| A | `ohara-engine` crate skeleton; `RetrievalEngine` wrapping `RepoHandle` cache |
| B | `EmbeddingCache`, `MetaCache`, `BlameCache` (P2–P4) |
| C | Unix-socket IPC: envelope types, length-prefixed framing, `Server` dispatch |
| D | `spawn_daemon`, file-locked `Registry`, `Client` transport, `find_or_spawn`, `try_daemon_call` |
| E | `BlameCache` wired into `explain_change` |
| F | `ohara serve` CLI subcommand |
| G | `ohara-mcp` rewired to use `RetrievalEngine`; envelope-parity goldens captured |
| H | `ohara query`/`explain` daemon routing; `ohara index` invalidation broadcast |
| I | E2E tests: happy path and fallback path (ignored; in-process daemon) |
