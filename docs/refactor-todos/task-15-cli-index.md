# Task 15: CLI scaffold + `ohara index` — refactor backlog

Captured at HEAD `64d5e2a`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–14 backlogs are not duplicated here. Items in Task 16+ proper scope
(real `query`/`status` implementation, MCP surface) are out of scope here.

---

### 1. `ohara_home()` panics when neither `HOME` nor `USERPROFILE` is set

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/mod.rs:13`
- **What:** `.expect("HOME or USERPROFILE")` aborts the process with a stack
  trace if both env vars are missing. Rare on developer machines, but
  realistic in CI sandboxes, minimal Docker images, systemd units with
  `Environment=` cleared, or weird Windows service contexts.
- **Why:** A stack-trace panic on a perfectly recoverable input shape is the
  CLI version of "silent corruption" — it looks like a bug in ohara when
  it's actually an environment problem. A typed error would let `main` print
  a friendly "set OHARA_HOME or HOME" message.
- **Suggestion:** Return `Result<PathBuf>`; bubble through `index_db_path`
  and the two callers in `index::run`. One-line `OhraError::InvalidInput`
  or `anyhow!("set OHARA_HOME, HOME, or USERPROFILE")`.
- **Effort:** XS

### 2. Cross-platform `ohara_home()` is untested on Windows

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/mod.rs:9-15`
- **What:** The `HOME` → `USERPROFILE` fallback handles stock Windows, but
  not service accounts with `%USERPROFILE%` unset, OneDrive-redirected
  profiles, or msys2/git-bash where `$HOME` is a Unix path that Windows
  APIs reject. The result feeds straight into `SqliteStorage::open`.
- **Why:** Plan 1 doesn't gate Windows, but the smoke test sets
  `OHARA_HOME` precisely because the default path is awkward.
- **Suggestion:** Swap the manual lookup for `dirs::home_dir()` (or
  `etcetera`); keep the `OHARA_HOME` override.
- **Effort:** XS

### 3. `.await??` in `spawn_blocking` chain is inscrutable

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/index.rs:22-24`
- **What:** `tokio::task::spawn_blocking(|| FastEmbedProvider::new()).await??`
  — the double-`?` flattens `JoinError` then the inner `Result`. Correct
  but reads as a typo to anyone unfamiliar with the pattern. Will recur
  for every "build a blocking thing on the runtime" call site (Task 17 MCP
  server is likely to need the same).
- **Why:** Pure readability; gets worse the more places it appears.
- **Suggestion:** A small helper in `commands/mod.rs`:
  `pub async fn build_blocking<F, T, E>(f: F) -> Result<T> where ...` that
  collapses the join error into `anyhow`. Or, add a constructor on
  `FastEmbedProvider` that runs the blocking work internally and returns a
  future.
- **Effort:** XS

### 4. `println!` for the index summary — should also emit a `tracing` event

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/index.rs:30-33`
- **What:** The completion summary uses `println!` to stdout while
  everything else in the binary logs through `tracing` to stderr. The
  mixed sink is correct for a one-shot CLI; the missing structured event
  isn't. Operators piping logs to an aggregator get no record of counts.
- **Why:** Task 17 MCP server output lives only in tracing; a future
  `--json` flag will need to swap the format anyway.
- **Suggestion:** Keep `println!` for humans; add a parallel
  `tracing::info!(new_commits, new_hunks, head_symbols, "indexed")`.
- **Effort:** XS

### 5. No tests for `commands::*` helpers beyond the gated smoke test

- **Severity:** Medium
- **Location:** `crates/ohara-cli/tests/index_smoke.rs:7-9`
- **What:** The only integration test is `#[ignore]`'d behind a fastembed
  network comment. `resolve_repo_id`, `ohara_home`, and `index_db_path`
  have zero direct coverage. A regression in `RepoId::from_parts` arg
  ordering or a swallowed `canonicalize` would slip through.
- **Why:** These helpers are the seam between the CLI and Plan 1. Task 16
  grows the surface; Task 17 (MCP) likely reuses `ohara_home()` — drift
  here turns into an MCP bug.
- **Suggestion:** Add `#[test]`s in `commands/mod.rs` for `ohara_home`
  (set/unset `OHARA_HOME` via `temp_env`) and `index_db_path` (assert
  `<home>/<repo_id>/index.sqlite`). Keep `resolve_repo_id` covered by a
  non-ignored test using `git2` (already a dev-dep) on a temp repo.
- **Effort:** S

### 6. Smoke test doesn't exercise `status::run` or assert on-disk artefacts

- **Severity:** Low
- **Location:** `crates/ohara-cli/tests/index_smoke.rs:25-27`
- **What:** The test calls `index::run` and stops — no SQLite-row check,
  no `status::run` (stubbed). The diff's own comment flags this for Task 16
  to revisit; recording so it doesn't slip through hand-off.
- **Suggestion:** When Task 16 lands: (a) drop `#[ignore]` if the model
  cache can live in CI, (b) call `commands::status::run`, (c) assert the
  index file exists under `OHARA_HOME` and is non-empty.
- **Effort:** S (when triggered)

### 7. `resolve_repo_id` opens a fresh `GitWalker` every CLI invocation

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/mod.rs:18-25`
- **What:** Each `index`/`query`/`status` call opens `GitWalker` solely to
  pull `first_commit_sha()`, then drops it; `Indexer::run` re-opens via
  `GitCommitSource::open`. For a 50k-commit repo, libgit2 `discover` is
  cheap (a few `stat`s) — redundancy is aesthetic, not perf-load-bearing.
- **Why:** Worth confirming, not fixing. Current shape keeps the CLI
  helper free of `Indexer` source types, which is the right separation.
- **Suggestion:** Defer. If a "load repo metadata once, reuse everywhere"
  pattern emerges (likely with Task 16 `status`), introduce a `RepoHandle`
  owning canonical path + repo-id + cached first-commit.
- **Effort:** S (when triggered)

### 8. `Indexer::run` re-walks the repo on every `ohara index`

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/index.rs:26-29`
- **What:** Each invocation builds a fresh `Indexer` and walks all
  commits + re-extracts HEAD symbols. Storage idempotency makes most of
  this a no-op, but tree-sitter parses still execute. Plan 2 may add
  explicit caching keyed on HEAD sha.
- **Why:** Not a bug — `ohara index` is meant to be re-runnable. But the
  CLI is the natural place to add a HEAD-unchanged short-circuit, and a
  future `--force` flag would slot here too.
- **Suggestion:** Defer to Plan 2. Read last-indexed HEAD sha from
  `repos`; early-return if equal and `--force` not set.
- **Effort:** M (when triggered)

---

### See also

- `cargo clippy -p ohara-cli --all-targets` — already clean at HEAD
  `64d5e2a`. No inherited lint warnings to flag here.
- Plan-aware: Task 16 (real `query` + `status` impls) replaces the stub
  bodies in `commands/query.rs` and `commands/status.rs`; items #5 and
  #6 become live the moment that lands.
- Plan-aware: Task 17–18 (MCP server, separate binary) will likely want
  to re-use `ohara_home()` and `index_db_path()` from the lib crate —
  items #1 and #3 (helper extraction) compound across both binaries.
- Plan-aware: Task 20 e2e exercises the full pipeline end-to-end and
  will give item #6 (smoke-test expansion) free coverage if structured
  similarly.
- Cross-task: item #4 (tracing on summary) continues the pattern noted
  in Task 11 #4, Task 12 #6, Task 13 #3, Task 14 #7.
- Time-sensitive: #1 before any Windows / minimal-container user surfaces
  it. #5 alongside Task 16. Anytime: rest.
