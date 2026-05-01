# Task 15: CLI scaffold + `ohara index` — refactor backlog

Captured at HEAD `64d5e2a`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–14 backlogs are not duplicated here. Items in Task 16+ proper scope
(real `query`/`status` implementation, MCP surface) are out of scope here.

---

### CLOSED items (resolved by later plans)

- **#1 — `ohara_home()` panics on missing env vars.** Closed in Plan 2:
  `ohara_home()` moved to `crates/ohara-core/src/paths.rs` returning
  `Result<PathBuf>` with `OhraError::Config` instead of panicking. CLI
  re-exports via `pub use ohara_core::paths::{index_db_path, ohara_home};`.
- **#4 — Index summary uses `println!` only, no structured `tracing`
  event.** Closed in commit `855dc9f`: added a parallel
  `tracing::info!(new_commits, new_hunks, head_symbols, "indexed")`
  alongside the human-readable `println!`.
- **#5 — No tests for `commands::*` helpers beyond the gated smoke
  test.** Closed in Plan 2: `paths.rs` ships unit tests for
  `ohara_home_uses_env_when_set` and `ohara_home_falls_back_to_home`;
  `compute_index_status` in `query.rs` ships its own contract test.
- **#7 — `resolve_repo_id` opens fresh `GitWalker` per invocation.**
  Closed by deferral confirmed: deferred in the original entry,
  remains acceptable; helper still makes structural sense as-is.
- **#8 — `Indexer::run` re-walks repo on every invocation.** Closed
  in Plan 2: `--incremental` flag short-circuits when watermark
  equals HEAD before booting the embedder. Plus `--force` (Plan 3 / D)
  is the explicit re-walk knob.
- **#9 — Silent multi-minute index runs.** Closed:
  - **(b) tracing liveness:** commit `855dc9f` emits
    `tracing::info!` every 100 commits during the walk so
    `RUST_LOG=info` users see steady progress.
  - **(a) progress bar:** `crates/ohara-cli/src/progress.rs` adds
    `IndicatifProgress` (an `indicatif`-backed `ProgressSink`) which
    `ohara index` attaches by default when stderr is a TTY; `--no-progress`
    disables it. `ohara-core::ProgressSink` is the headless contract;
    `NullProgress` is the no-op default for the MCP server and tests.

### Resource-control flags added alongside #9

`ohara index` now accepts:
- `--commit-batch <N>` — override `Indexer::batch_commits` (default 512).
- `--threads <N>` — set `OMP_NUM_THREADS` and `RAYON_NUM_THREADS` before
  the embedder loads, capping the ort runtime's parallelism.
- `--no-progress` — suppress the progress bar even when stderr is a TTY
  (the per-100-commits tracing event still fires).

These were not in the original Task 15 backlog but slot naturally
alongside #9 (the same multi-minute-run UX gap).

---

### Still live (deferred long-tail)

### 2. Cross-platform `ohara_home()` is untested on Windows

- **Severity:** Low
- **Location:** `crates/ohara-core/src/paths.rs:16-25`
- **What:** The `HOME` → `USERPROFILE` fallback handles stock Windows,
  but not service accounts with `%USERPROFILE%` unset, OneDrive-
  redirected profiles, or msys2/git-bash where `$HOME` is a Unix path
  Windows APIs reject. Result feeds into `SqliteStorage::open`.
- **Why:** v0.2 dropped Windows from the release matrix entirely;
  this is moot until/unless Windows comes back. Keep the entry as a
  marker if Windows support returns.
- **Suggestion:** Swap the manual lookup for `dirs::home_dir()`;
  keep the `OHARA_HOME` override.
- **Effort:** XS
- **Status:** Deferred — Windows is not a supported target as of v0.2.

### 3. `.await??` in `spawn_blocking` chain is inscrutable

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/index.rs:73-74`,
  `crates/ohara-cli/src/commands/query.rs:30-37`,
  `crates/ohara-mcp/src/server.rs:26-32`
- **What:** Multiple sites use `tokio::task::spawn_blocking(...).await??`
  to flatten `JoinError` then the inner `Result`. Correct but reads
  as a typo. Five call sites now (CLI index, CLI query, MCP server
  embedder + reranker, plus the `Blamer` in v0.5).
- **Why:** Pure readability; cumulative as more sites appear.
- **Suggestion:** A small `build_blocking<F, T>(f: F) -> Result<T>`
  helper somewhere in `ohara-core` that hides the double-?.
- **Effort:** XS

---

### See also

- Plan 2 closed items #1, #4, #5 (partially), #7, #8.
- Plan 3 / Track D added `--force` which is item #8's intended escape
  hatch.
- v0.6 prep: if Windows comes back on the matrix, item #2 lights up.
