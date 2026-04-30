# Task 11: git walker — refactor backlog

Captured at HEAD `d45b482`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–10 backlogs are not duplicated here. Items in Task 12+ proper scope
(`hunks_for_commit`, async `CommitSource` impl wrapping `spawn_blocking`) are
out of scope here.

---

### 1. `clippy::map_flatten` warning at `walker.rs:37`

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/walker.rs:36-39`
- **What:** `c.parent_count().checked_sub(1).map(|_| c.parent(0).ok()).flatten().map(...)`
  triggers `clippy::map_flatten`. Kept verbatim from plan per instructions.
- **Why:** The only clippy warning in `ohara-git` today. Also, the
  `checked_sub(1).map(|_|...)` guard is redundant — `c.parent(0).ok()`
  already returns None for parentless commits.
- **Suggestion:** Replace with `c.parent(0).ok().map(|p| p.id().to_string())`.
  One-liner; clears the warning and simplifies.
- **Effort:** XS

### 2. `Sort::TIME | Sort::REVERSE` in `first_commit_sha` is fragile

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/walker.rs:17-22`
- **What:** Uses author-time ordering to find the root commit. Author time
  can be non-monotonic under rebase, cherry-pick, and clock-skew commits
  imported from other repos. The "first" commit by author time may not be
  the topological root.
- **Why:** `first_commit_sha` feeds `RepoId = hash(first_commit_sha +
  canonical_path)` per spec §5 / §3 invariants — Task 14+ derives repo
  identity from this. A flaky root sha means `RepoId` could change between
  indexing runs on repos with non-monotonic history, splitting indexes.
  `Sort::TOPOLOGICAL | Sort::REVERSE` walks parents-before-children
  deterministically and yields the parentless root regardless of timestamps.
- **Suggestion:** Switch to `Sort::TOPOLOGICAL | Sort::REVERSE`, or walk
  ancestors of HEAD and pick the first commit with `parent_count() == 0`.
  Add a doc comment noting the `RepoId`-stability rationale.
- **Effort:** XS (sort flag swap + comment) / S (parentless-walk variant)

### 3. `list_commits` returns `Vec<CommitMeta>` — non-streaming

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/walker.rs:24-49`
- **What:** Materialises the full commit list before returning. On a
  100k-commit repo (~150 B per `CommitMeta` baseline + message), that's
  tens of MB resident, with messages dominating.
- **Why:** Fine for v1 scale, but Task 12's indexer batches ~512 commits
  per txn — a streaming iterator fits that naturally and decouples walker
  memory from repo size. Cheaper to add the seam now than retrofit later.
- **Suggestion:** Add `fn iter_commits(&self, since: Option<&str>) -> Result<impl
  Iterator<Item = Result<CommitMeta>> + '_>`; keep `list_commits` as a
  `iter_commits().collect()` wrapper. Task 12 indexer adopts iter form.
- **Effort:** S

### 4. No `tracing::debug!` instrumentation in walker

- **Severity:** Low
- **Location:** `crates/ohara-git/src/walker.rs` (entire file); `tracing` is
  already a workspace dep.
- **What:** No spans or events. Indexer logs at the outer level only, so
  walker-internal timing (revwalk setup, per-commit `find_commit`) is
  invisible when diagnosing slow indexing.
- **Why:** When Task 12+ wires `CommitSource` through `spawn_blocking` and
  someone reports "indexing is slow", we'll want a span around `list_commits`
  and a debug counter for commits walked. Cheap to add up-front.
- **Suggestion:** `#[tracing::instrument(skip(self))]` on `list_commits` and
  `first_commit_sha`; `tracing::debug!(count = out.len(), "walked commits")`
  before returning. Skip instrumenting per-commit (too noisy).
- **Effort:** XS

### 5. `c.author().name().unwrap_or("")` silently drops non-UTF8 names

- **Severity:** Low
- **Location:** `crates/ohara-git/src/walker.rs:43`
- **What:** `Signature::name()` returns `None` for non-UTF8 bytes. Walker
  maps None → empty → filtered out. Repos with legacy latin-1 / windows-1251
  author metadata lose attribution silently.
- **Why:** Acceptable for v1 (no author filtering yet), but a future "show
  commits by X" query would mysteriously miss those commits.
- **Suggestion:** Either fall back to `c.author().name_bytes()` with
  `String::from_utf8_lossy`, or doc the limitation on `list_commits`.
  Document for now; lossy when author search lands.
- **Effort:** XS (doc) / S (lossy fallback + test)

### 6. `CommitMeta::parent_sha` first-parent-only semantics undocumented

- **Severity:** Low
- **Location:** `crates/ohara-git/src/walker.rs:24` (no doc on `list_commits`);
  `crates/ohara-core/src/types.rs:78` (no doc on the field).
- **What:** Walker records `c.parent(0)` only; merge commits' second-parent
  is silently dropped. Spec §5 schema comment says "first parent only;
  merges noted via flag" — that intent isn't echoed in the Rust types or
  the walker API.
- **Why:** A reader inspecting `CommitMeta::parent_sha` on a merge commit
  would reasonably expect *a* parent, not specifically the first. Without
  a doc comment the convention is invisible until someone reads the SQL
  schema.
- **Suggestion:** Add `///` to `list_commits` and to `CommitMeta::parent_sha`
  noting first-parent-only and `is_merge` as the flag for ≥2 parents.
- **Effort:** XS

### 7. `init_repo_with_commits` test fixture will need sharing in Task 12+

- **Severity:** Low
- **Location:** `crates/ohara-git/src/walker.rs:60-78`
- **What:** Linear-history fixture. Task 12 (hunks) and future walker tests
  (merges, branches, renames) will want the same scaffolding plus extensions.
- **Why:** Premature to hoist now (single use site, YAGNI). Worth flagging
  so Task 12 implementer hoists deliberately rather than copy-pasting.
- **Suggestion:** On second use, hoist to `crates/ohara-git/tests/common/mod.rs`
  (or `src/test_support.rs` behind `#[cfg(test)]`) with per-commit file
  specs. Do NOT hoist now — wait for the second caller.
- **Effort:** S (when triggered by Task 12)

### 8. Module/type docs missing on `walker.rs` and `GitWalker`

- **Severity:** Low
- **Location:** `crates/ohara-git/src/walker.rs:1-9`
- **What:** No `//!` module doc, no `///` on `GitWalker` or its methods.
  Same gap Task 8/9/10 backlogs flagged elsewhere.
- **Why:** `Repository::discover` (vs `open`) walks upward looking for
  `.git/` — non-obvious. `first_commit_sha` (#2) and `list_commits`
  watermark semantics (#6) belong on the API doc, not in the impl.
- **Suggestion:** Add `///` to each method covering: `open` does ancestor
  discovery; `first_commit_sha` is for `RepoId` derivation; `list_commits`
  with `since` returns commits *strictly newer than* the watermark.
  Roll #2/#5/#6 doc fixes into this pass.
- **Effort:** XS

---

### See also

- `cargo clippy -p ohara-git --all-targets` flags only #1
  (`clippy::map_flatten` at `walker.rs:37`). Pre-existing `ohara-core`
  warnings (`unused_imports` in `indexer.rs`/`retriever.rs`, `dead_code` on
  `Indexer.embed_batch` and `Retriever.{storage,embedder}`) belong to
  Tasks 3–4 / Task 9 backlog, not here.
- Spec drift: `parent_sha` first-parent-only matches spec §5 schema comment.
  Walker is consistent with spec; only the Rust-side doc-comment is missing
  (item #6).
- Plan-aware notes: #3 (streaming) and #7 (shared fixture) become actionable
  in Task 12. #2 (`first_commit_sha` stability) becomes actionable in
  Task 14+ when `RepoId` derivation lands. #4 (tracing) is most useful once
  `CommitSource` is consumed from the indexer.
- Time-sensitive: #1 (clippy noise), #2 (before Task 14+). Anytime: rest.
