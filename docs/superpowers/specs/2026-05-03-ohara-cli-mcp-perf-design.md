# ohara CLI / MCP performance design

**Status:** Phase 3 shipped in v0.7.5 via plan-16; Phase 2 / Phase 4 remain TBD.

**Goal:** make `ohara query` and `ohara explain` (CLI) and `find_pattern` /
`explain_change` (MCP) feel "blazingly fast" — sub-second cold path on the
standalone CLI, sub-100ms p50 on the warm path. Indexing latency is
explicitly out of scope.

## Why this matters

Today a cold `ohara query` invocation takes >10 seconds. The four
dominant cost centers are:

1. **Embedder cold-load** — `FastEmbedProvider::new` mmaps BGE-small
   (~80MB ONNX). Multi-second on first call.
2. **Reranker cold-load** — `FastEmbedReranker::new` mmaps
   `bge-reranker-base` (~110MB ONNX). Multi-second on first call. On
   the very first call after install, this is also a download.
3. **Sequential cold-start** — `query.rs` opens storage, loads the
   embedder, loads the reranker one after another. They could be
   parallel.
4. **Per-call MCP overhead** — `compatibility_status` and
   `index_status_meta` each walk the repo on every `find_pattern`;
   `get_hunk_symbols` is called in a per-hit loop.

The MCP server (long-running) only pays (1)–(3) once at boot, but the
CLI pays them every invocation. Most user-facing slowness lives in the
CLI cold path; some lives in the per-call hot path that affects both
surfaces.

## Shape

Four phases, executed in order:

1. **Phase 1 — Perf harness + tracing.** Foundation. Every later PR
   must show before/after numbers from the harness.
2. **Phase 2 — Standalone CLI wins.** Independent optimizations to the
   short-lived CLI process. No protocol changes.
3. **Phase 3 — `ohara serve` daemon.** Long-running per-version
   daemon that the CLI auto-spawns; eliminates cold-start for
   subsequent invocations. Modeled on Gradle's daemon pool.
4. **Phase 4 — Per-call optimizations.** Improvements to the
   in-process `RetrievalEngine`; benefit the daemon and the MCP
   server equally.

A new crate `ohara-engine` is introduced in Phase 3 to host the shared
core (`RetrievalEngine`, caches, per-repo storage handle pool). Per
`CONTRIBUTING.md` §dependency-direction, `ohara-core` MUST NOT depend
on `ohara-storage` / `-embed` / `-git` concretes; `ohara-engine` is
the wiring layer that does.

## Architecture (post-Phase-3)

```
                      ┌────────────────┐
                      │ ohara-cli      │ thin client OR standalone fallback
                      └────────┬───────┘
                               │  Unix socket (when daemon up)
                               ▼
   ┌──────────────────────────────────────────────────────┐
   │  ohara-engine::RetrievalEngine                       │
   │  - Storage + EmbeddingProvider + RerankProvider      │
   │  - Blamer per-repo                                   │
   │  - EmbeddingCache (LRU on query embeddings)          │
   │  - BlameCache (LRU on (repo, file, blob))            │
   │  - meta-memo (5s TTL on compat_status, index_status) │
   └──────────────────────────────────────────────────────┘
                               ▲
                               │  rmcp stdio transport
                               │
                      ┌────────┴───────┐
                      │ ohara-mcp      │ unchanged user-visible behavior
                      └────────────────┘
```

The MCP server stops constructing its own retriever and instead wraps
a `RetrievalEngine`. The daemon binary `ohara serve` wraps the same
engine behind a Unix-socket transport adapter.

## Phase 1 — Perf harness + tracing

### Tracing layer (`ohara-core` + `ohara-engine`)

A `tracing::Span` per phase, with a uniform attribute schema:

```
phase           = "embed_query" | "lane_knn" | "lane_fts_text"
                | "lane_fts_sym_hist" | "lane_fts_sym_head" | "rrf"
                | "rerank" | "hydrate_symbols"
                | "compatibility_check" | "index_status_meta"
                | "storage_open" | "embed_load" | "rerank_load" | "blame"
elapsed_ms      = number
hit_count       = number  (lanes / rerank)
cache           = "hit" | "miss"  (cached operations)
```

Activated by `RUST_LOG=ohara=info,phase=trace` or a new `--trace-perf`
CLI flag. The flag installs a `tracing-subscriber` layer that dumps
per-span elapsed times to stderr in compact form.

### Per-method storage metrics (Layer 2)

`ohara-storage` wraps each `Storage` trait method with `AtomicU64`
counters: `call_count`, `total_elapsed_us`, `rows_returned`. One
atomic add per call. A `Storage::metrics_snapshot()` method exposes
the snapshot.

### Per-statement SQL trace (Layer 1, opt-in)

`rusqlite::Connection::trace` callback enabled by
`RUST_LOG=ohara_storage::sql=trace`. Off by default — adds constant
overhead per statement. When on, logs each SQL with elapsed time.

### Perf harness binary

Lives in `tests/perf/bin/`. Operator-run, not in CI (per
`CONTRIBUTING.md`). Two binaries:

- `cli_query_bench.rs` — spawns `ohara query` N times, records cold
  vs warm. Drives both standalone and (post-Phase-3) daemon paths.
- `mcp_query_bench.rs` — drives `OharaService::find_pattern` /
  `explain_change` directly without rmcp framing.

Output: JSON to `target/perf/runs/<git_sha>-<utc>.json` with phase
histograms. A small `perf_diff.rs` compares two run files for PR
descriptions.

### Fixtures

- `fixtures/tiny/repo` (existing) — sanity check, regression guard.
- `fixtures/medium/repo` (new) — `fixtures/build_medium.sh` shallow-
  clones `ripgrep` at a frozen tag (TBD; pinned to a SHA in the
  script). ~5k commits. Numbers from this fixture are comparable
  across machines and across PRs.

### No CI gating

Per `CONTRIBUTING.md`, perf harness is operator-run. Numbers cited in
PR descriptions are committed run files; no CI pass/fail.

## Phase 2 — Standalone CLI wins

Each is a standalone PR with harness numbers.

**P2.1 — Parallelize cold-start I/O.** `query.rs` and `explain.rs`
today open storage → load embedder → load reranker sequentially.
`tokio::join!` all three. The two ONNX loads happen in parallel
`spawn_blocking` tasks. SQLite open overlaps for free.

**P2.2 — Transport-aware reranker default.** Standalone CLI defaults
to `no_rerank=true`; opt in with `--rerank`. MCP defaults are
unchanged (`no_rerank=false`). On the first call without
`--rerank`, the CLI prints a one-line stderr notice:

```
note: rerank skipped for speed. enable with --rerank.
```

The notice is suppressible via `OHARA_QUIET=1` or `--quiet`. When
Phase 3 lands, the notice text is extended to mention the daemon
path (`... or `ohara serve` for warm rerank by default`); a CLI
invocation that finds an idle daemon proxies to it and gets the
reranker on by default. The standalone fallback is then the only
path that defaults rerank off.

**P2.3 — `EmbeddingCache`.** New `LruCache<(ModelId, blake3(query_text)),
Arc<Vec<f32>>>`, capacity 256. Wraps `EmbeddingProvider::embed_batch`
inside `RetrievalEngine`. No-op for standalone CLI (fresh engine per
invocation); benefit appears in Phase 3.

**P2.4 — Batched `get_hunk_symbols`.** Replace the per-hit loop in
`Retriever::find_pattern_with_profile` with one call to a new
`Storage::get_hunk_symbols_batch(repo_id, &[HunkId]) ->
HashMap<HunkId, Vec<HunkSymbol>>`. Single SQL with `IN (?, ?, …)`.

**P2.5 — Coalesce MCP meta calls.** Replace separate
`compatibility_status` + `find_pattern_with_profile` +
`index_status_meta` invocations with a single
`engine.run_find_pattern(query) -> (hits, profile, meta)`. Internally
the engine memoizes meta fragments with a 5s TTL.

**P2.6 — Truncate diff text into rerank.** Cap rerank input at the
first ~80 lines or ~4KB per candidate. Cross-encoder cost is roughly
quadratic in input length. Eval-gated against the existing plan-10
harness; ship only if no measurable nDCG@5 regression.

**P2.7 — SQLite pragma + statement-cache audit.** Confirm
`mmap_size`, `cache_size`, `journal_mode=WAL`, `synchronous=NORMAL`,
`temp_store=MEMORY` are set; confirm prepared statements are reused
across calls. Likely small; included for completeness.

**Estimated combined Phase 2 impact:** cold CLI from ~10s → ~1–2s
on a primed model cache. Floor is the embedder mmap; the daemon path
in Phase 3 takes it to <50ms.

## Phase 3 — `ohara serve` daemon (Gradle-inspired)

> **Implementation:** plan-16, shipped v0.7.5.

### Crate layout change

A new `crates/ohara-engine/` is introduced. Owns:

- `RetrievalEngine` — wraps `Storage`, `EmbeddingProvider`,
  `RerankProvider`, `Blamer`, plus the caches.
- Per-repo `Storage` handle cache (one daemon, multi-repo).
- Per-repo `Blamer` cache.
- `EmbeddingCache`, `BlameCache`, meta-memo.

`ohara-mcp` is rewritten to construct a `RetrievalEngine` and call
its methods. The new binary `ohara serve` does the same and exposes
the engine over a Unix socket.

### Daemon model

| Gradle concept | Ohara translation |
|---|---|
| Per-version daemon, multi-project | Per-version daemon, multi-repo |
| `~/.gradle/daemon/<v>/registry.bin` | `${XDG_CACHE_HOME}/ohara/daemon/<ohara_version>/registry.json`; mac: `~/Library/Caches/...` |
| Auto-spawn on first call | CLI: read registry → pick idle compatible daemon → if none, double-fork detached `ohara serve --socket <path> --pid-file <path>` and wait on a readiness file (10s timeout) |
| Idle timeout (3 hr) | 30 min default, configurable via `OHARA_DAEMON_IDLE_TIMEOUT` env |
| `--stop` / `--status` | `ohara daemon stop|status|list` subcommands |
| `--no-daemon` opt-out | Same flag; auto-disabled when `CI=true` (override via `OHARA_FORCE_DAEMON=1`) |
| Version compatibility | Registry keyed by `ohara_version`; clients only pick same-version daemons |

Concurrency model differs from Gradle: one daemon serves multiple
concurrent connections via tokio tasks. The reranker is serialized
behind a `tokio::sync::Semaphore(1)` (revisit to `(2)` only if
profiling warrants). Storage is concurrency-safe via SQLite WAL.

### Registry shape

```json
{
  "daemons": [
    {
      "pid": 12345,
      "socket_path": "/run/user/1000/ohara/0.7.0-abc123.sock",
      "ohara_version": "0.7.0",
      "ohara_git_sha": "abc123",
      "started_at_unix": 1714694400,
      "last_health_unix": 1714694450,
      "busy": false
    }
  ]
}
```

File-locked via `fs2::FileExt::lock_exclusive` for read-modify-write
cycles. Stale entries (`kill -0 pid` fails OR `last_health_unix` >
5 min old) are pruned on every read.

### Socket path

- Linux: `${XDG_RUNTIME_DIR}/ohara/<ohara_version>-<random8>.sock`,
  mode `0600`. Falls back to `${TMPDIR}/ohara-<uid>/...` when
  `XDG_RUNTIME_DIR` is unset.
- macOS: `${TMPDIR}/ohara-<uid>/<ohara_version>-<random8>.sock`,
  mode `0600`.
- Windows: not a target platform; not handled.

### IPC protocol

Length-prefixed JSON. Frame: `[u32 BE length][JSON bytes]`. One
request per connection (no socket-level multiplexing — concurrent
calls open separate connections, daemon spawns a tokio task per
accepted connection).

Request envelope:

```json
{
  "id": "req-1",
  "method": "find_pattern" | "explain_change" | "ping" | "shutdown"
          | "invalidate_repo" | "index_status" | "metrics",
  "repo_path": "/absolute/path/to/repo",
  "params": { /* method-specific */ }
}
```

Response envelope:

```json
{ "id": "req-1", "ok": true,  "result": { ... } }
{ "id": "req-1", "ok": false, "error": { "code": "NEEDS_REBUILD" | "NO_INDEX" | "INTERNAL", "message": "..." } }
```

`params` for `find_pattern` and `explain_change` mirror the existing
`PatternQuery` / `ExplainQuery` types so the same serde structs cover
MCP rmcp and daemon socket transports.

### Refresh contract

`ohara index` actively notifies. At the end of a successful index
run, it sends `invalidate_repo {repo_path}` to every registered
daemon. Daemons drop the named repo's cached `IndexStatus`,
`compatibility_status`, and any cached `BlameCache` entries for
files in that repo. SQLite rows themselves are not cached in the
daemon (storage goes through SQLite each call), so no DB-level
invalidation is needed.

### Failure modes & fallbacks

- Daemon spawn fails or readiness file never appears → CLI logs a
  one-line stderr warning, falls back to standalone path.
- Socket connect fails after registry says daemon is up → CLI
  prunes the stale entry, retries spawn once, falls back to
  standalone path on second failure.
- Daemon panics mid-request → CLI's read returns EOF; CLI re-attempts
  with a fresh daemon spawn; if the same query panics twice, the
  error is surfaced to the user.

### CLI subcommands

```
ohara daemon status   # tabular: PID, version, started_at, idle_for, repo_count
ohara daemon stop     # sends `shutdown` to all running daemons
ohara daemon list     # raw registry contents
```

## Phase 4 — Per-call optimizations

All live inside `RetrievalEngine`; benefit daemon, MCP, and the
standalone CLI when the engine is reused across calls.

**P4.1 — Blame-result LRU.** New `BlameCache: LruCache<(RepoId,
RepoPath, ContentHash), Arc<Vec<BlameLine>>>`, capacity 64.
Invalidated by `invalidate_repo`. Removes blame as the dominant
cost of `explain_change` on iterative `--lines` refinement.

**P4.2 — Smaller reranker (eval-gated).** Evaluate `bge-reranker-v2-m3`
and `jina-reranker-v1-tiny-en` against `bge-reranker-base` on the
plan-10 eval harness. Ship only if a candidate matches or exceeds
the baseline on nDCG@5. If none qualify, keep the current model.

**P4.3 — Reduce `rerank_top_k`.** Default drops from 50 → 30.
Eval-gated against plan-10 the same way as P4.2. Marginal value of
candidates 30–50 is small after RRF.

**P4.4 — Concurrent rerank batching.** Profile-driven. Only ship if
benchmarks show clear wins under daemon-concurrent load — otherwise
intra-op ONNX parallelism is sufficient.

**P4.5 — `compatibility_status` short-TTL memoization.** 5s TTL on
`compatibility_status` and `IndexStatus` inside the engine.
Invalidated immediately by `invalidate_repo`. Removes per-call
repo-walk overhead in the daemon and MCP server. Lands here if not
already pulled into Phase 2 (P2.5).

**P4.6 — Lane SQL prepared-statement audit.** Confirm `bm25_hunks_*`
and `knn_hunks` reuse `rusqlite::CachedStatement` rather than
re-parsing per call. Likely already correct; included to confirm.

**P4.7 — SQLite db-status snapshot.** Add `Storage::db_status()`
returning `SQLITE_DBSTATUS_CACHE_HIT/MISS/USED`. Surfaced via the
new `metrics` IPC method and `ohara daemon metrics` subcommand.
Tells us whether `cache_size` is sized right.

**P4.8 — Truncate diff text into rerank** (deferred from Phase 2 if
not landed there). Same change, same eval gate.

**P4.9 — KNN backend re-evaluation (decision-only, conditional).**
Run after Phase 3 lands. If `lane_knn` is in the top 3 hot-path
phases at p50 on the ripgrep fixture, open a separate design doc
covering `usearch` in-process HNSW alongside sqlite-vec, INT8/binary
quantization within sqlite-vec, waiting for sqlite-vec native HNSW,
or an external vector DB. If `lane_knn` is not in the top 3, no
work happens — single-file local-first storage is preserved.

**Phase 4 completion criteria** (on the ripgrep fixture, daemon
warm):

- `find_pattern` **with rerank**: p50 < 100ms.
- `find_pattern` **no-rerank**: p50 < 30ms (this is the standalone-
  CLI-via-daemon default — IPC + 4 lane queries + RRF, no model
  forward pass).
- `explain_change`: p50 < 200ms cold blame / < 50ms warm blame
  (warm = `BlameCache` hit).

## Constraints

- **No protocol breakage at MCP boundary.** `find_pattern` and
  `explain_change` JSON envelopes — `hits`, `_meta`, `_meta.hint`,
  `_meta.compatibility`, `_meta.query_profile`, `_meta.explain` —
  stay byte-identical for clients. The engine refactor is internal.
- **Indexing latency is out of scope.** No changes to the index path
  (`Indexer::run`) except: emit `invalidate_repo` notifications in
  Phase 3, and respect the new `EmbeddingCache` if it shows up on
  the index side (it doesn't — query-only).
- **Local-first storage preserved.** sqlite-vec stays the KNN backend
  unless P4.9 surfaces a clear hot-path bottleneck.
- **`CONTRIBUTING.md` dependency direction holds.** `ohara-core` does
  not depend on `ohara-storage` / `-embed` / `-git`. The new
  `ohara-engine` crate is the only place that wires concretes
  besides the binaries.
- **Unix-only daemon.** Windows is not a target. Standalone path is
  the only path on platforms without Unix sockets, and we don't ship
  on those today.
- **No new top-level `*.md` files.** Spec lives under
  `docs/superpowers/specs/` per `CONTRIBUTING.md`.
- **All SQL stays in `ohara-storage`.** Including the new
  `get_hunk_symbols_batch` and `db_status` methods.
- **Eval gate.** Any change that could regress retrieval quality
  (P2.6, P4.2, P4.3, P4.8) ships with plan-10 eval numbers in the
  PR description.

## Test strategy

- **Unit tests** for the new `RetrievalEngine` cache layer (LRU
  eviction, `invalidate_repo` semantics, TTL boundaries).
- **Unit tests** for the registry: stale-entry pruning, version
  filtering, file-lock contention.
- **Integration tests** for the daemon: spawn → connect → find_pattern
  → invalidate_repo → idle-timeout shutdown.
- **MCP behavioral parity tests** — re-run the existing
  `crates/ohara-mcp/tests/` fixtures against the engine-backed
  rewrite to confirm response envelopes are byte-identical.
- **Perf harness runs** committed alongside any PR that claims a
  latency win, in the format described in Phase 1.

## Rollout

Phases ship in order. Each phase is independently mergeable:

- After **Phase 1**, no user-visible change but every later PR is
  measurable.
- After **Phase 2**, standalone CLI cold path drops from ~10s to
  ~1–2s. No protocol changes, no new binaries, no new flags beyond
  `--rerank` and `--quiet`.
- After **Phase 3**, the `ohara serve` daemon is available; CLI
  auto-detects it. Warm `ohara query` (with rerank by default via
  daemon path) drops to ~150–250ms (rerank-bound). The no-rerank
  flow drops to <30ms. Standalone fallback remains for
  `--no-daemon` and CI.
- After **Phase 4**, daemon-warm `find_pattern` with rerank hits
  p50 < 100ms (rerank tuning + smaller-reranker eval), no-rerank
  stays <30ms, and the engine ships per-method metrics, blame
  caching, and an optionally smaller reranker (eval-gated).

## Future work (explicitly deferred)

- KNN backend swap (P4.9 decision tree).
- Streaming responses (returning hits as they arrive).
- Daemon-side Windows support (named pipes).
- Multi-daemon scaling à la Gradle (one daemon per concurrent
  user). Current single-daemon-with-tokio-tasks model is expected
  to suffice for this workload.
- Reranker output caching beyond the query-embedding LRU.
