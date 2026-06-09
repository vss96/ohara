# ohara daemon consolidation — single shared engine host

Date: 2026-06-09
Status: approved (brainstorm 2026-06-09)
Related: plan-16 (`ohara serve` daemon), issue #58 (lazy reranker)

## Problem

Every MCP client session spawns its own `ohara-mcp` process, and each one
builds a full in-process `RetrievalEngine` with an **eagerly loaded
embedder** (`crates/ohara-mcp/src/server.rs::OharaServer::open`). The
plan-16 daemon infrastructure (Unix-socket IPC, registry, idle timeout,
spawn-with-readiness) exists but is only used by `ohara query` and
`ohara explain` — never by the MCP server.

Measured on a dev machine with **one** Claude Code session open
(2026-06-09):

| Process | RSS | Source |
|---|---|---|
| `ohara-mcp` v0.7.4 (plugin cache) | 1,395 MB | plugin MCP registration |
| `ohara-mcp` (`~/.cargo/bin`) | 921 MB | duplicate manual MCP registration |
| node wrapper | 41 MB | plugin launcher |

≈2.3 GB for a single session; N sessions multiply the per-session
processes. Compounding causes:

1. `ohara-mcp` never uses the daemon; one full engine per session, no
   idle timeout — it lives as long as the session.
2. The embedder loads at MCP boot even for sessions that never call a
   tool (`explain_change` needs no models at all).
3. `find_or_spawn_daemon` (`crates/ohara-engine/src/client/discover.rs`)
   is find→spawn→register without holding the registry lock across the
   sequence — concurrent cold starts can each spawn a daemon.
4. The registry path is **per-version**
   (`…/ohara/daemon/<version>/registry.json`), so after an upgrade,
   old-version daemons become invisible orphans until their idle
   timeout fires.
5. `Registry::pick_compatible` skips `busy` daemons. Nothing sets
   `busy: true` in production code today, but if anything ever did, a
   busy daemon would cause clients to spawn duplicates.
6. `ohara index` — including every post-commit hook run — loads the
   full embedder fresh each run (transient RSS spike + 2-5 s model-load
   latency per commit).

## Goals

- N concurrent MCP sessions (any mix of clients) share **exactly one**
  engine-hosting daemon process per ohara version, converging to one
  per machine.
- `ohara-mcp` boot does no model loading; per-session RSS < 50 MB.
- Steady-state daemon footprint follows a **tiered unload** policy:
  quantized embedder stays warm, reranker unloads after idle, whole
  daemon exits after longer full idle.
- Post-commit incremental indexing stops paying a per-run model load
  (wave 2).

## Non-goals

- Cross-version daemon sharing (a 0.9 client never talks to a 0.8
  daemon; the index-compat story stays as-is).
- Remote/TCP daemons; Unix sockets only.
- Replacing the in-process fallback. CI (`CI=true` without
  `OHARA_FORCE_DAEMON`) and `--no-daemon`/`OHARA_NO_DAEMON` keep
  working without a daemon.

## Design

### Phase A1 — `ohara-mcp` becomes a thin daemon client

- Tool handlers (`crates/ohara-mcp/src/tools/`) route through
  `try_daemon_call` + `find_or_spawn_daemon`, mirroring
  `crates/ohara-cli/src/commands/query.rs`. The IPC protocol already
  carries `FindPattern` and `ExplainChange`
  (`crates/ohara-engine/src/ipc/envelope.rs`); no new methods needed.
- **Daemon binary resolution.** `find_or_spawn_daemon` spawns
  `<binary> serve …`. Plugin installs ship only the `ohara-mcp`
  tarball, so the `ohara` CLI may be absent. Fix: `ohara-mcp` gains a
  `serve` mode (same `ServeArgs` surface) and clients pass
  `std::env::current_exe()` as the spawn binary. The serve runner
  (embedder construction + watchdog + registry heartbeat wiring,
  currently in `crates/ohara-cli/src/commands/serve.rs`) moves into
  `ohara-engine` so both binaries call one function. Registry records
  stay binary-agnostic: a CLI-spawned daemon serves MCP clients and
  vice versa (compatibility keyed on version, as today).
- **Lazy in-process fallback.** When the daemon is unavailable or
  disabled, `ohara-mcp` builds the in-process engine **lazily on the
  first tool call that needs it**. The embedder gets the same lazy
  treatment the reranker got in #58. `explain_change` through the
  fallback must not load any model.
- MCP server boot becomes near-instant (no model download/load at
  session start).

### Phase A2 — singleton hardening, version sweep, busy-flag removal

- **Atomic find-or-spawn.** Hold the registry exclusive file lock
  (`Registry::with_locked`) across the whole
  pick-compatible → spawn → register sequence. Contenders block on the
  lock (spawn readiness is bounded at 10 s), then re-run pick and find
  the registered daemon. No more duplicate cold-start spawns.
- **Version sweep.** On daemon start, scan sibling version dirs under
  `…/ohara/daemon/`, send best-effort `Shutdown` to alive records with
  a different version, and prune their registry files. Old daemons
  also still die via idle timeout; the sweep makes upgrade cleanup
  deterministic.
- **Remove the `busy` flag** from `DaemonRecord` and the busy-skip from
  `pick_compatible`. The daemon handles each connection in its own
  task (`crates/ohara-engine/src/server.rs`); concurrency is already
  per-connection. Clients must never spawn a second daemon because the
  first is mid-query.

### Phase A3 — tiered unload (memory policy)

- `LazyFastEmbedReranker` (`crates/ohara-embed/src/fastembed.rs`)
  currently uses `OnceCell`: load-once, never unloads. Replace with a
  reloadable slot (`RwLock<Option<FastEmbedReranker>>` + last-used
  timestamp). A daemon-side watchdog drops the reranker session after
  idle (default **600 s**, flag `--reranker-idle-secs`, `0` disables).
  Next rerank reloads transparently (~1-2 s penalty).
- The embedder stays warm for the daemon's lifetime.
- Whole-daemon idle exit stays at the existing default (1800 s,
  `--idle-timeout`), which drops everything.

### Phase A4 (wave 2) — `Embed` over IPC for incremental indexing

- New `RequestMethod::Embed { texts }` → embedding vectors response.
  Batches respect the existing `embed_batch` sizing; large batches are
  chunked to respect IPC frame limits.
- `ohara index` uses the daemon embedder when the run is small (the
  post-commit hook case): heuristic — commits-to-index ≤ ~50 routes to
  the daemon, larger runs (initial index, rebuild) stay in-process and
  own their models. Flag override: `--embed-via-daemon[=false]`.
- Wins: no per-hook model load (latency + transient RSS), and the
  daemon embedder stays warm for queries.

## Error handling

- Daemon connect/IPC failure → log at `debug`, fall back in-process
  (existing `try_daemon_call` semantics). A daemon crash mid-call
  surfaces as an IPC error → one reconnect attempt, then fallback.
- Stale registry entries are pruned by liveness checks
  (`list_alive`), as today.
- Registry lock contention during concurrent cold start: bounded by
  spawn readiness timeout (10 s); on timeout the contender falls back
  in-process rather than erroring the tool call.

## Testing

- Registry: concurrent `find_or_spawn_daemon` from two processes →
  exactly one daemon registered (lock-held sequence test).
- Version sweep: fixture registry dirs for two versions → old daemon
  shut down and pruned.
- Reranker unload: injected clock; assert session dropped after idle
  and transparently reloaded on next rerank.
- MCP integration: tool call with a live fake daemon (socket fixture) →
  served over IPC; daemon disabled → lazy in-process fallback;
  `explain_change` fallback loads no model (assert via provider spy).
- Perf guard (operator-run, `tests/perf/`): MCP boot wall-time and RSS
  ceiling; two concurrent MCP servers → single daemon process.

## Success criteria

- One `ohara`-version daemon process regardless of session count.
- `ohara-mcp` per-session RSS < 50 MB; boot < 200 ms.
- Steady-state daemon RSS ≈ embedder-only footprint once the reranker
  idle-unloads.
- Post-commit hook run (1 commit) completes without a model load
  (after A4).

## Out-of-band (no code, operator action)

- The dev machine has duplicate MCP registrations (plugin + manual
  `~/.cargo/bin/ohara-mcp`); removing one halves session memory today.
- The plugin pin at v0.7.4 predates the quantized embedder; bumping to
  0.9.x shrinks the largest observed process immediately (tracked in
  the usability spec, B1).
