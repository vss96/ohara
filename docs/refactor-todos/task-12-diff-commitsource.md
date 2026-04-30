# Task 12: diff + GitCommitSource — refactor backlog

Captured at HEAD `e6878a0`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–11 backlogs are not duplicated here. Items in Task 13+ proper scope
(tree-sitter parsing, symbol extraction) are out of scope here.

---

### 1. `Repository::discover` re-runs on every `hunks_for_commit` call

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/lib.rs:46-57`
- **What:** Each `spawn_blocking` task opens a fresh `git2::Repository`
  via `Repository::discover`. For N commits, that's N opens + N pack-index
  loads. On a 10k-commit repo, tens of seconds of pure overhead.
- **Why:** `Repository` is `!Sync` (the field-drop rationale in the doc)
  but *is* `Send`. A `Mutex<Repository>` amortises discovery across the
  pass. Indexer is single-threaded per repo, so contention is nil.
- **Suggestion:** Replace `repo_path: PathBuf` with
  `Arc<Mutex<Repository>>` (validate `Send`) and reuse in both trait
  methods. Keep `walker()` as the fresh-open accessor.
- **Effort:** S

### 2. Diff callback always returns `true` — no error short-circuit

- **Severity:** Low
- **Location:** `crates/ohara-git/src/diff.rs:51`
- **What:** The `diff.print` callback unconditionally returns `true`.
  `git2`'s callback contract uses the bool as "continue iteration";
  `false` aborts. Today the closure is infallible, but any future
  fallible step (binary detect, size cap) needs an error-capture pattern.
- **Why:** Silent-`true` hides bugs once fallible work is added; the
  outer `?` only sees errors from `print` itself, not the closure body.
- **Suggestion:** Add a doc comment on `hunks_for_commit` noting the
  contract; any new fallible step must capture via `Option<Error>` and
  return `false`, then `?` after `print`.
- **Effort:** XS (doc) / S (capture pattern)

### 3. `current.as_ref().unwrap().1` in hunk-grouping is awkward

- **Severity:** Low
- **Location:** `crates/ohara-git/src/diff.rs:37-44`
- **What:** The match arm `Some((p, _)) if *p != path` flushes the
  previous file by reaching back into `current.as_ref().unwrap().1` to
  fetch the `ChangeKind`. The `_` binding is shadowed and the `unwrap`
  duplicates the guard. Hard to read, easy to break under future refactor.
- **Why:** Pure readability; the logic is correct. Future maintainers
  adding a third file-level field (size, mode, etc.) will trip on this.
- **Suggestion:** Bind both fields in the guard: `Some((p, prev_ck)) if
  *p != path => { hunks.push(make_hunk(sha, p, *prev_ck, std::mem::take(&mut buf))); ... }`.
  Drops the `unwrap`.
- **Effort:** XS

### 4. `DiffOptions` skips rename detection — renames split as Delete+Add

- **Severity:** Medium
- **Location:** `crates/ohara-git/src/diff.rs:16-22`
- **What:** `DiffOptions::new()` defaults to no rename/copy detection.
  The `git2::Delta::Renamed` arm in the switch is dead code without a
  `diff.find_similar(...)` call; renames surface as `Deleted` old +
  `Added` new.
- **Why:** Plan 1 doesn't need rename tracking. Plan 2's lineage queries
  (Retriever cross-commit reasoning) break at every rename without it.
  Cheap to flag now, expensive after a deployed index.
- **Suggestion:** After building the diff, call
  `diff.find_similar(Some(&mut FindOptions::new().renames(true)))` —
  one-liner. Or document deferral until lineage lands.
- **Effort:** XS (enable) / S (enable + rename test)

### 5. `from_utf8(line.content()).unwrap_or("")` silently drops binary diffs

- **Severity:** Low
- **Location:** `crates/ohara-git/src/diff.rs:50`
- **What:** Non-UTF8 content (binaries, latin-1 source files) decodes to
  empty string. The hunk record is still emitted with the file path,
  language, and change-kind — but `diff_text` is empty, so its embedding
  is meaningless and FTS gets nothing.
- **Why:** Acceptable for v1 (binaries are noise for code-search). But a
  silent empty-string hunk pollutes the index with low-signal rows that
  `knn_hunks` will still rank. Better to skip explicitly than to embed
  zeroes.
- **Suggestion:** Detect with `git2::DiffFile::is_binary()` on the delta
  and skip the file entirely; or use `String::from_utf8_lossy` so at
  least non-UTF8 text survives. Document the chosen behaviour.
- **Effort:** XS

### 6. No `tracing` instrumentation in `hunks_for_commit` or async wrapper

- **Severity:** Low
- **Location:** `crates/ohara-git/src/diff.rs:6` (sync); `crates/ohara-git/src/lib.rs:33,46` (async wrappers).
- **What:** Neither the sync `hunks_for_commit` nor the async
  `CommitSource` impl methods carry `#[tracing::instrument]` or debug
  events. When indexing slows down, there's no per-commit visibility —
  only the outer `tracing::info!` at indexer level.
- **Why:** Same rationale as Task 11 backlog #4: cheap up-front, painful
  to retrofit when diagnosing a slow `cargo run -- index` against a
  large repo. `spawn_blocking` boundaries especially benefit from spans.
- **Suggestion:** `#[tracing::instrument(skip(repo), fields(sha = %sha))]`
  on the sync `hunks_for_commit`; `#[tracing::instrument(skip(self))]`
  on both async impl methods. Add a `tracing::debug!(hunk_count = ...)`
  before returning. Cross-reference Task 11 #4.
- **Effort:** XS

### 7. `walker()` accessor signature change — no external callers (yet)

- **Severity:** Low
- **Location:** `crates/ohara-git/src/lib.rs:25-28`
- **What:** Plan 12 specified `walker(&self) -> &GitWalker`; shipped as
  `walker(&self) -> Result<GitWalker>` because `Repository: !Sync`
  blocked storing the walker. Workspace grep confirms zero external
  callers today. Task 15's CLI uses `GitWalker::open` directly.
- **Why:** Recording so future-readers don't waste cycles reasoning
  about plan-vs-code drift. Each call also re-pays repo discovery; see #1.
- **Suggestion:** No code change. Extend the existing doc comment to
  note "fresh `Repository` discovery per call — cache externally in
  loops". Cross-link #1.
- **Effort:** XS

### 8. Test fixture overlaps Task 11's `init_repo_with_commits` — second use

- **Severity:** Low
- **Location:** `crates/ohara-git/src/diff.rs:88-106` (new); `crates/ohara-git/src/walker.rs:60-78` (existing).
- **What:** Two diverging in-tree fixtures build linear-history repos.
  Task 11 backlog #7 set the trigger as "hoist on second use" — this
  *is* the second use. Task 13+ adds merge / rename / multi-file fixtures.
- **Why:** Avoid drift accumulating across 3-4 local helpers.
- **Suggestion:** Hoist to `crates/ohara-git/tests/common/mod.rs` (or
  `src/test_support.rs` behind `#[cfg(test)]`). Generalise to
  `&[(filename, content)]` per commit; migrate both call sites.
- **Effort:** S

---

### See also

- `cargo clippy -p ohara-git --all-targets` is clean at HEAD; no warnings
  to track here.
- Plan-aware notes: #1 becomes pressing once Task 14's Retriever and
  Task 15's CLI consume `GitCommitSource` against real-size repos.
  Task 15's CLI calls `GitWalker::open` directly (not via the accessor),
  so #7's API change is invisible to that path. #4 (rename detection)
  is the highest-leverage flag for Plan 2's lineage queries — re-eval
  before that work starts.
- Time-sensitive: #1 (before first large-repo indexing run), #4 (before
  Plan 2 lineage). Anytime: rest.
- Cross-task: #6 (tracing) and #8 (shared fixture) are continuations of
  Task 11 backlog items #4 and #7 respectively.
