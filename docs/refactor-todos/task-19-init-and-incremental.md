# Task 19: `ohara init` + `ohara index --incremental` — refactor backlog

Captured at HEAD `eba97d6`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–18 backlogs are not duplicated here, only cross-referenced. Plan 3+
proper scope is out of scope. `cargo clippy --workspace --all-targets` is
clean at this HEAD; `cargo test --workspace` is 48 passed / 6 ignored / 0
failed.

This task encompasses Plan 2 in full: shared `ohara-core::paths`,
`compute_index_status` + `CommitsBehind`, `OharaServer::embedder` field
drop, `rmcp = "=0.1.5"` pin, `--incremental` fast path, and `ohara init`
(hook + optional CLAUDE.md stanza). All Plan-1 carry-overs landed; see
"Plan-1 carry-overs verified" at the bottom.

---

### 1. `--write-claude-md` has no test for stale-stanza replacement

- **Severity:** Medium
- **Location:** `crates/ohara-cli/tests/e2e_init.rs:138-161` (idempotency)
- **What:** The hook side has `init_replaces_managed_block_in_place`
  (lines 264-302) which seeds a stale managed block and asserts the new
  body replaces it. The CLAUDE.md side has no equivalent — the
  idempotency test only re-runs init twice with the same stanza, which
  doesn't exercise the `replace_block(...)` path with *different* old
  content. If the markers ever change, or the replace logic regresses
  (e.g. only writes if begin marker found at offset 0), the bug ships
  silent.
- **Why:** Test plan §6 lists three CLAUDE.md cases (no file / our
  markers / unmarked content) and the implementation honors all three,
  but only two are observed by tests. Symmetry with the hook tests is
  cheap to add.
- **Suggestion:** Mirror `init_replaces_managed_block_in_place` for
  CLAUDE.md: pre-seed a file with markers around `stale stanza body`,
  run init with `write_claude_md: true`, assert stale text is gone and
  fresh `## ohara` body present.
- **Effort:** XS

### 2. `--force` flag has zero test coverage

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/init.rs:43-46`,
  `crates/ohara-cli/tests/e2e_init.rs` (all eight tests pass `force:
  false`)
- **What:** `--force` is a documented user-facing flag (overwrite a
  hook even if it lacks markers) but no test exercises `force: true`.
  The branch in `write_hook` (line 113: `if !path.exists() || force`)
  is reached only via the `!path.exists()` half in tests.
- **Why:** A regression that flipped `force` to a no-op would ship.
  Plan 2 §2 explicitly justifies `--force` ("user wants a clean
  slate") so it's not a stub feature.
- **Suggestion:** Add `init_force_replaces_unmanaged_hook`: pre-write
  a hook with `echo something` and no markers, run init with `force:
  true`, assert the file is now `#!/bin/sh\n<managed block>\n` with no
  trace of the user content.
- **Effort:** XS

### 3. `head_commit_sha()` has no in-crate test in `walker.rs`

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/walker.rs:31-35`
- **What:** New O(1) helper introduced by deviation 3 in the coder
  notes. The fast-path's correctness depends on this returning the
  same SHA that `Indexer::run` ultimately writes to
  `set_last_indexed_commit`. Indirectly exercised by
  `incremental_at_head_is_noop_and_skips_embedder_init`, but that
  test is `#[ignore]`'d (downloads embedder model) so default
  `cargo test` does not cover the new method.
- **Why:** The whole companion methods (`first_commit_sha`,
  `list_commits` with/without `since`) have direct unit tests
  (lines 99-139). `head_commit_sha` is the lone exception. A bug that
  returned (e.g.) `head.target()` instead of `head.peel_to_commit()`
  would slip past CI on a default `cargo test`.
- **Suggestion:** Add a 5-line `head_commit_sha_returns_tip_of_head`
  test alongside `first_commit_sha_walks_to_topological_root`, using
  the existing `init_repo_with_commits` helper and asserting equality
  against `cs.last().sha`.
- **Effort:** XS

### 4. `e2e_incremental.rs` runs everything `#[ignore]`'d behind a
process-global mutex

- **Severity:** Medium
- **Location:** `crates/ohara-cli/tests/e2e_incremental.rs:12-20`
- **What:** Coder noted item — the file holds a static `Mutex<()>`
  across `await` points (with `#[allow(clippy::await_holding_lock)]`)
  because `OHARA_HOME` is a process-global env var. All three tests
  in this file are also `#[ignore]`'d (they boot the FastEmbed model).
  The mutex is correct (cargo runs tests in parallel within a binary
  by default), but it's a smell: env-var serialization and embedder
  cost are both encoded as test-time concerns, not addressed at the
  source.
- **Why:** Same constraint hits `paths.rs` unit tests
  (lines 31-43 there reinvent the same mutex pattern). Three serialized
  test sites is the canonical extract trigger. A `TestEnvHome` RAII
  guard in a shared test-utils module would fix both.
- **Suggestion:** Add `crates/ohara-core/src/test_util.rs` (or a tiny
  `dev-dependency` crate) exposing a `with_ohara_home(&Path) -> Guard`
  that takes the mutex, sets the env var, and restores it on drop.
  Convert both call sites. Also unblocks future tests that need the
  same pattern.
- **Effort:** S

### 5. `incremental_on_fresh_repo_indexes_everything` and
`incremental_after_partial_index_only_walks_new_commits` were green
when written

- **Severity:** Low
- **Location:** `crates/ohara-cli/tests/e2e_incremental.rs:39-128`
- **What:** Coder deviation 1 — only test 11 was genuinely red; tests
  9 & 10 captured already-passing behavior (the v0.1 indexer was
  already incremental via `last_indexed_commit`). Plan §6 listed all
  three as "NEW tests" without distinguishing.
- **Why:** Characterization tests are useful (regression nets) but
  the plan's Step 8 framing implied red-then-green for all three.
  Worth recording so future plan/work-log audits don't flag this as
  a TDD discipline lapse — it's a plan-tightness issue, not an
  implementation issue. Tests 9-10 are still valuable as regression
  nets if the indexer ever loses incrementality.
- **Suggestion:** None for the test code. For plan-writing
  discipline: when a test characterizes already-working behavior,
  call it "characterization" in the plan and pair it with at least
  one driver-test (which 11 was). Recommend amending Plan 2 §6 with
  a one-line annotation, or document the pattern in
  `docs/superpowers/skills/writing-plans/...` if a relevant skill
  exists.
- **Effort:** XS (note); no code change.

### 6. Hook has no timeout — runaway `ohara` blocks the commit

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/init.rs:20-24`
  (`HOOK_BODY`)
- **What:** The hook is fail-closed against missing/broken `ohara`
  (`command -v` guard + `|| true`), but if `ohara` is on PATH and
  hangs (deadlock, slow network on a future remote-index feature,
  pathological repo), the post-commit hook blocks the commit
  indefinitely. There's no `timeout 30` wrapper, no background
  detach.
- **Why:** v0.2's `--incremental` is fast (msec on no-op, sec on
  small deltas), so this is theoretical today. But Plan 2 §2's "never
  blocks the commit" justification will need revisiting once
  ohara's index pass grows (e.g. tree-sitter on large diffs in
  Plan 3) or if a future feature adds network IO.
- **Suggestion:** Investigate `( cd ... && ohara index --incremental
  >/dev/null 2>&1 ) &` (background detach — but loses error visibility
  and complicates testing) vs. `timeout 30 ohara index --incremental`
  (requires GNU coreutils `timeout`, not POSIX). Defer until the index
  pass actually grows; document the constraint in the hook comment
  meanwhile.
- **Effort:** S (when triggered)

### 7. Plan 2's "no GitWalker dep in `walker.rs`" specifies wrong
location for `GitCommitsBehind`

- **Severity:** Low
- **Location:** `crates/ohara-git/src/lib.rs:81-112` (where it lives),
  Plan 2 Step 7 (where the plan said it would live)
- **What:** Plan 2 §7 says "add `GitCommitsBehind` adapter in
  `crates/ohara-git/src/walker.rs`". Implementer placed it in
  `lib.rs` instead, alongside `GitCommitSource` (which has the same
  shape: trait impl wrapping `GitWalker`). This is a strictly better
  placement — `lib.rs` is the adapter layer; `walker.rs` is the
  primitive. But the deviation isn't documented anywhere except
  this file.
- **Why:** Future plan readers comparing plan text to landed code
  may be confused. Pairs with Task 18 #3 (plan-errata pattern).
- **Suggestion:** When Plan 3 adds its own plan-errata block,
  back-fill a one-line entry in Plan 2's "Files coder will touch"
  noting the relocation. No code change.
- **Effort:** XS (note)

### 8. `Indexer::embed_batch` is still dead with `#[allow(dead_code)]`

- **Severity:** Low
- **Location:** `crates/ohara-core/src/indexer.rs:28-29, 38`
- **What:** Coder flagged item — the field is set in `Indexer::new`
  (line 38: `embed_batch: 32`) but never read. Comment on line 27-28
  describes it as "Reserved knob for capping per-batch embedder calls;
  not yet wired into the loop". The current loop processes one
  commit's hunks at a time, so the cap is implicit at hunk-count.
- **Why:** Same anti-pattern as Task 18 #1's `OharaServer::embedder`
  field. `#[allow(dead_code)]` on a reserved-for-future field tends
  to outlive its planned usage; if the design decision is "we don't
  need this any more", removing it is honest.
- **Suggestion:** Either (a) wire the knob into the inner loop (chunk
  the per-commit text vector by `embed_batch` before calling
  `embed_batch`); (b) delete the field with a one-line note in commit
  msg. Defer until a real batching constraint surfaces (very large
  commits with hundreds of hunks).
- **Effort:** S (a) / XS (b)

### 9. `retriever.rs` has `#[cfg(test)] use` items mixing test and
production code

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:4-5`
- **What:** Coder flagged item from the final-pass clippy fixes.
  `use crate::types::{CommitMeta, Hunk};` is gated `#[cfg(test)]`
  because production code only references them indirectly through
  `HunkHit`. Clean from clippy's POV but unusual — most files put
  test-only imports inside the `#[cfg(test)] mod tests {}` block,
  not at module top level.
- **Why:** Module-level `#[cfg(test)] use` works (rustc accepts it)
  but reads as a smell — a future reader scanning the import block
  has to mentally apply the gate. The conventional fix is to move
  the imports inside the `mod tests` block.
- **Suggestion:** In `mod tests`, replace the `use super::*;`
  expansion with explicit `use crate::types::{CommitMeta, Hunk};`
  alongside it. Delete the top-level gated imports.
- **Effort:** XS

### 10. Hook test `post_commit_hook_invokes_ohara_index_on_synthetic_commit`
requires `git` binary on PATH

- **Severity:** Low
- **Location:** `crates/ohara-cli/tests/e2e_init.rs:198-262`
- **What:** Coder deviation 2 — test uses `Command::new("git")`
  because libgit2 commits don't fire hooks. Gated on `#[cfg(unix)]`,
  but a Unix CI runner without git installed (a stripped-down Docker
  image) would panic on `.expect("git add")`. Most realistic CI
  images ship git, but this is the first ohara test with such a hard
  external dependency.
- **Why:** Worth flagging for the next CI config change. If we ever
  switch to a minimal `rust:alpine`-style runner, this test breaks
  with a useless panic message instead of `#[ignore]`'ing cleanly.
- **Suggestion:** Wrap the `Command::new("git")` call in a
  `which::which("git")` probe (or shell out with
  `Command::new("git").arg("--version")`) and skip with
  `eprintln!("git not on PATH; skipping"); return;` if absent.
  Alternative: gate the test on a feature flag the CI workflow sets.
- **Effort:** XS

---

### Plan-1 carry-overs verified

- ✅ `git grep "embedder:" crates/ohara-mcp/` → only the local binding
  inside `OharaServer::open()` (`server.rs:25`); no struct field.
- ✅ Workspace `Cargo.toml:42` → `rmcp = { version = "=0.1.5",
  features = ["server", "transport-io"] }`.
- ✅ Both `OHARA_HOME` inline copies removed; `ohara_core::paths::
  ohara_home()` is the single source. CLI re-exports via
  `commands/mod.rs:12`; MCP calls `ohara_core::paths::index_db_path`
  directly at `server.rs:21`.
- ✅ Both `compute_index_status` call sites:
  `crates/ohara-cli/src/commands/status.rs:18` and
  `crates/ohara-mcp/src/server.rs:44`. Inline duplication deleted at
  both sites — verified against the diff in commit `930147f`.

### See also

- `cargo clippy --workspace --all-targets` — clean at HEAD `eba97d6`.
- `cargo test --workspace` — 48 passed / 6 ignored / 0 failed at HEAD
  `eba97d6` (matches the 54-tests claim).
- Inherited from Task 17 (closed by this task): #1 (extract
  `commits_behind` — done as `compute_index_status` + `CommitsBehind`
  trait); #2 (shared `OHARA_HOME` — done as `ohara-core::paths`).
- Inherited from Task 18 (closed by this task): #1 (drop dead
  `OharaServer::embedder` field — done in commit `0acf38a`); #2
  (pin `rmcp = "=0.1.5"` — done in `9935c7b`).
- Plan-aware: Plan 3's `explain_change` will re-walk
  `OharaService`/`ServerHandler`; Task 18 backlog items #4-#8 still
  apply there. The CLAUDE.md stanza writer added here is the seed
  for Plan 3's discoverability work — see Task 18 #7 about
  `discoverability.rs` consolidation.
- Time-sensitive: items #1, #2, #3 before Plan 3 starts (testability
  hygiene). #4 anytime (test-utils crate is reusable). #5, #7 are
  one-line notes; resolve in next plan-writing pass. #6, #10 only
  when CI/perf actually triggers.
