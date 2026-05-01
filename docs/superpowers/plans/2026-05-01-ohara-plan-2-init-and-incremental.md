# ohara v0.2 — `init` + auto-freshness + Plan-1 cleanup

> **For agentic workers:** Use superpowers:subagent-driven-development or superpowers:executing-plans to implement task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ohara stay current automatically (post-commit git hook + `--incremental` fast path) and bundle four Plan-1 carry-over cleanups (shared OHARA_HOME, shared index_status, drop dead embedder field, pin rmcp).

**Architecture:** New shared `paths` module + new `CommitsBehind` trait both in `ohara-core` (still git-free / io-light). New `commands::init` in `ohara-cli` writes a marker-fenced post-commit hook + (optionally) a marker-fenced CLAUDE.md stanza. `ohara index --incremental` adds a fast-path early-return when HEAD == watermark.

**Tech Stack:** Rust 2021, clap, async-trait, std::fs, git2 (already a dev-dep for tests), rmcp pinned `=0.1.5`.

---

## 0. Findings that shape the design

- `IndexStatus` already exists at `crates/ohara-core/src/query.rs:30` with the right shape. Do **not** redefine it.
- `ohara_home()` already exists at `crates/ohara-cli/src/commands/mod.rs:9` but is **duplicated** inline in `crates/ohara-mcp/src/server.rs:25-28`. Cleanup: move canonical impl into `ohara-core` and call from both.
- `index_status` logic (storage status + `commits_behind_head` from the walker + hint) implemented twice: `crates/ohara-cli/src/commands/status.rs:13` and `crates/ohara-mcp/src/server.rs:44` (`index_status_meta`). Same shape, slightly different output (CLI prints; MCP returns `ResponseMeta`). Extract shared core; both callers wrap it.
- `GitWalker::list_commits(Some(sha))` at `crates/ohara-git/src/walker.rs:30` already does the incremental walk: `Sort::TOPOLOGICAL | Sort::REVERSE`, `walk.hide(oid)` excludes the watermark and ancestors. There is **no** non-incremental code path to remove; "incremental" today just means "pass the watermark." So `--incremental` is a UX/contract flag, not a new algorithm.
- `Storage::get_index_status(...).last_indexed_commit` already returns the max-indexed SHA. Adding `Storage::max_indexed_commit_sha` would be redundant. Skip it; use the existing API.
- `OharaServer::embedder` (server.rs:14) is dead — `Retriever` (core/retriever.rs:23) owns its own `Arc<dyn EmbeddingProvider>` clone. Field can be removed without lifetime impact.

---

## 1. Shared helpers (host crate: `ohara-core`)

New file `crates/ohara-core/src/paths.rs`, re-exported from `lib.rs`:

```rust
pub fn ohara_home() -> Result<PathBuf> {
    if let Ok(s) = std::env::var("OHARA_HOME") { return Ok(PathBuf::from(s)); }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| OhraError::Config("HOME or USERPROFILE not set".into()))?;
    Ok(PathBuf::from(home).join(".ohara"))
}
pub fn index_db_path(id: &RepoId) -> Result<PathBuf> {
    Ok(ohara_home()?.join(id.as_str()).join("index.sqlite"))
}
```

Note the signature change: today `commands::ohara_home()` panics via `.expect("HOME or USERPROFILE")`; the v0.2 version returns `Result<PathBuf>` per the brief. Add `OhraError::Config(String)` variant in `core/error.rs`.

Shared `index_status` lives in `ohara-core` but is **git-aware via a trait**, so core stays git-free. Use the existing `CommitSource`-style pattern: add a tiny trait in `ohara-core/src/query.rs`:

```rust
#[async_trait]
pub trait CommitsBehind: Send + Sync {
    async fn count_since(&self, since: Option<&str>) -> Result<u64>;
}

pub async fn compute_index_status(
    storage: &dyn Storage,
    repo_id: &RepoId,
    behind: &dyn CommitsBehind,
) -> Result<IndexStatus> {
    let st = storage.get_index_status(repo_id).await?;
    let n = behind.count_since(st.last_indexed_commit.as_deref()).await?;
    Ok(IndexStatus {
        last_indexed_commit: st.last_indexed_commit,
        commits_behind_head: n,
        indexed_at: st.indexed_at,
    })
}
```

`IndexStatus` itself is unchanged (already in `query.rs`). `ohara-git` adds a thin `GitCommitsBehind` adapter wrapping `GitWalker::list_commits(...).len()`. CLI `status::run` and MCP `index_status_meta` both call `compute_index_status` and then format the result. The "hint" stays in MCP only (presentation concern).

## 2. `ohara init` command

**Location**: `crates/ohara-cli/src/commands/init.rs`, registered in `main.rs::Cmd::Init(commands::init::Args)`.

**Clap shape**:
```rust
#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Also append/update an "ohara" stanza in CLAUDE.md.
    #[arg(long)]
    pub write_claude_md: bool,
    /// Overwrite an existing post-commit hook even if it lacks the ohara marker.
    #[arg(long)]
    pub force: bool,
}
```

**Idempotency rule (justified)**: detect a marker line `# >>> ohara managed (do not edit) >>>` / `# <<< ohara managed <<<`. Three cases:
1. No hook file → write our hook with marker block + shebang.
2. Hook file exists and contains our markers → replace the block between markers in place; leave anything outside untouched.
3. Hook file exists, no markers → **append** our managed block (separated by a blank line). Justification: refusing breaks people who already have a custom post-commit hook (e.g. ctags); appending is conservative and reversible (delete the marker block to uninstall). `--force` lets a user replace the whole file in the rare case they want a clean slate.

**Hook body** (POSIX sh, never blocks the commit):
```sh
#!/bin/sh
# >>> ohara managed (do not edit) >>>
# Re-index this repo on every commit. Silently skipped if `ohara` is not on PATH.
if command -v ohara >/dev/null 2>&1; then
  ( cd "$(git rev-parse --show-toplevel)" && ohara index --incremental >/dev/null 2>&1 ) || true
fi
# <<< ohara managed <<<
```

The `( cd … )` subshell makes the hook robust under `git -C` invocations. Trailing `|| true` plus `command -v` guard ensure a missing or broken `ohara` binary never fails the commit. After write, `chmod 0755`.

## 3. `--write-claude-md`

Target file: `<repo>/CLAUDE.md` (create if absent). Stanza markers `<!-- ohara:start -->` / `<!-- ohara:end -->`. If markers exist, **replace** the block. If not, **append** (preceded by `\n\n`). Never modify content outside the markers.

Stanza body:
```
<!-- ohara:start -->
## ohara

This repo is indexed by [ohara](https://github.com/vss96/ohara). Use the `find_pattern` MCP tool to ask "how have we solved X before?" — it returns ranked commits with diff excerpts and provenance.

- Index updates automatically via the `post-commit` hook installed by `ohara init`.
- Manual refresh: `ohara index --incremental`.
- Status: `ohara status`.
<!-- ohara:end -->
```

## 4. `ohara index --incremental`

**Clap**: add `#[arg(long)] pub incremental: bool` to `commands::index::Args`. Semantics:

- Today's behavior is already incremental (the indexer reads `status.last_indexed_commit` and only walks newer). So `--incremental` is a **contract flag**: when set, exit `0` with no work if the watermark equals HEAD (skip even loading the embedder — fast path for the post-commit hook). When unset, behavior is unchanged from v0.1.
- **No new Storage trait method.** The watermark already comes from `Storage::get_index_status(...).last_indexed_commit`. Adding `max_indexed_commit_sha` would duplicate that.
- Fast path in `index::run` when `args.incremental`:
  1. Open storage, read status.
  2. Resolve HEAD SHA via `GitWalker`.
  3. If `status.last_indexed_commit == Some(head)`, log "up-to-date" and return `Ok(())` **before** spawning the FastEmbed model.

## 5. rmcp pin

In root `Cargo.toml`, change line 41 from:
```
rmcp = { version = "0.1", features = ["server", "transport-io"] }
```
to:
```
rmcp = { version = "=0.1.5", features = ["server", "transport-io"] }
```
The `=` operator is exact-match (cargo). No other crate manifests change (they all use `rmcp.workspace = true`).

## 6. Test plan (NEW tests)

New file `crates/ohara-cli/tests/e2e_init.rs`:
1. `init_creates_post_commit_hook_in_fresh_repo` — tempdir + `git init`, run `commands::init::run`, assert `.git/hooks/post-commit` exists, is `0755`, contains both marker lines and the `command -v ohara` guard.
2. `init_is_idempotent_when_run_twice` — run twice, assert exactly one marker pair, file size unchanged second time.
3. `init_appends_to_existing_unmanaged_hook` — pre-create `post-commit` with `echo custom`, run init, assert original line is still present and our marker block follows.
4. `init_replaces_managed_block_in_place` — pre-create a hook containing stale content between our markers, run init, assert stale content is gone and markers contain the current body.
5. `init_with_write_claude_md_creates_file` — assert `CLAUDE.md` created with markers and stanza.
6. `init_write_claude_md_is_idempotent` — run twice, assert one marker pair and content equal between runs.
7. `init_write_claude_md_preserves_other_content` — pre-write `CLAUDE.md` with `# Project rules\n...`, run init, assert original content untouched and stanza appended.
8. `post_commit_hook_invokes_ohara_index_on_synthetic_commit` — install hook, point `PATH` at a shim script that writes a sentinel file, make a commit via `git2`, assert sentinel exists. (This test does **not** depend on the real `ohara` binary being built; it verifies the hook *invokes* something named `ohara`.)

New tests in `crates/ohara-cli/tests/index_smoke.rs` (or split into `e2e_incremental.rs`):
9. `incremental_on_fresh_repo_indexes_everything` — assert `report.new_commits == total commits`.
10. `incremental_after_partial_index_only_walks_new_commits` — index N commits, add 2 more commits, run `--incremental`, assert `report.new_commits == 2`.
11. `incremental_at_head_is_noop_and_skips_embedder_init` — run twice; assert `report.new_commits == 0` on second.

New unit tests in `ohara-core`:
12. `paths::ohara_home_uses_env_when_set` and `paths::ohara_home_falls_back_to_home`.
13. `query::compute_index_status_combines_storage_and_walker` — with a fake `Storage` and fake `CommitsBehind`.

## 7. Order of work (TDD red/green; commit after each test write and each impl)

- [ ] **Step 1: pin rmcp.** Edit root `Cargo.toml`. `cargo build -p ohara-mcp`. Single commit.
- [ ] **Step 2: drop dead `OharaServer::embedder` field.** Edit `crates/ohara-mcp/src/server.rs` (remove field + `#[allow(dead_code)]`; the local `embedder` binding inside `open()` stays — the `Retriever::new` clone keeps the model alive). `cargo test -p ohara-mcp`.
- [ ] **Step 3: add `OhraError::Config` variant.** Edit `crates/ohara-core/src/error.rs`.
- [ ] **Step 4 (red):** add unit tests for `ohara_home`, `index_db_path` in a new `crates/ohara-core/src/paths.rs`. Commit (red).
- [ ] **Step 5 (green):** implement `paths.rs`, re-export from `crates/ohara-core/src/lib.rs`. Update `crates/ohara-cli/src/commands/mod.rs` to re-export from core (delete the duplicate). Update `crates/ohara-mcp/src/server.rs` to call `ohara_core::paths::ohara_home()` (delete the inline block). Commit (green).
- [ ] **Step 6 (red):** add unit test in `crates/ohara-core/src/query.rs` for `compute_index_status` using fake `Storage` + fake `CommitsBehind`. Commit.
- [ ] **Step 7 (green):** implement trait + free function in core; add `GitCommitsBehind` adapter in `crates/ohara-git/src/walker.rs`; rewrite `crates/ohara-cli/src/commands/status.rs` and `crates/ohara-mcp/src/server.rs::index_status_meta` to call it. `cargo test --workspace`. Commit.
- [ ] **Step 8 (red):** add tests 9–11 to `crates/ohara-cli/tests/`. Commit.
- [ ] **Step 9 (green):** add bool to `crates/ohara-cli/src/commands/index.rs::Args`; implement HEAD-equals-watermark fast path before embedder init. Commit.
- [ ] **Step 10 (red):** add `crates/ohara-cli/src/commands/init.rs` with `run` returning `unimplemented!()`; wire into `commands/mod.rs` and `main.rs`; create `crates/ohara-cli/tests/e2e_init.rs` with tests 1–4. Commit (red).
- [ ] **Step 11 (green):** implement marker-aware writer in `commands::init`; constants for marker strings and hook body live in `init.rs`. Commit (green).
- [ ] **Step 12: `--write-claude-md` (red→green):** add tests 5–7 in `e2e_init.rs` (red); implement marker-aware CLAUDE.md writer in `commands::init` (green). Two commits.
- [ ] **Step 13: Final pass:** `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. Update `README.md` only if a new top-level command warrants a `## Setup` line.

## Files coder will touch

- `Cargo.toml` (workspace)
- `crates/ohara-core/src/error.rs`
- `crates/ohara-core/src/paths.rs` (new)
- `crates/ohara-core/src/query.rs`
- `crates/ohara-core/src/lib.rs`
- `crates/ohara-git/src/walker.rs`
- `crates/ohara-cli/src/main.rs`
- `crates/ohara-cli/src/commands/mod.rs`
- `crates/ohara-cli/src/commands/init.rs` (new)
- `crates/ohara-cli/src/commands/index.rs`
- `crates/ohara-cli/src/commands/status.rs`
- `crates/ohara-cli/tests/e2e_init.rs` (new)
- `crates/ohara-cli/tests/index_smoke.rs` (or new `e2e_incremental.rs`)
- `crates/ohara-mcp/src/server.rs`
