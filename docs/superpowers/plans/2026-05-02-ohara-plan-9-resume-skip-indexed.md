# ohara v0.6.3 — skip-already-indexed plan

> **For agentic workers:** the RFC at
> `docs/superpowers/specs/2026-05-02-ohara-v0.6.3-resume-skip-rfc.md`
> is the contract. This is a small, well-bounded fix: one storage
> method, one indexer-loop check, one regression test, one doc note.
> TDD throughout; commit per red/green step per project memory
> conventions.

**Goal:** stop the indexer from re-embedding commits that already
have a `commit_record` row when resume walks reach them via a
non-watermark-ancestor path. Targets v0.6.3.

**Architecture:** add `Storage::commit_exists(sha)` (cheap PK lookup),
short-circuit in `Indexer::run`'s per-commit loop, regression test
in the indexer test module.

**Tech stack:** Rust, sqlx-style sqlite (rusqlite via existing
`ohara-storage`), tokio, anyhow.

---

## Phase 1 — Storage method

### Task 1.1 — `commit_exists` on the Storage trait

**Files:**
- Modify: `crates/ohara-core/src/storage.rs` (the trait def)
- Modify: `crates/ohara-storage/src/lib.rs` (the SqliteStorage impl)
- Modify: `crates/ohara-storage/src/tables/commit.rs` (raw query)

- [x] **Step 1: Write the failing trait-level test.** In
  `crates/ohara-core/src/indexer.rs` (where the existing
  `MockStorage` lives in `#[cfg(test)] mod tests`), extend the mock
  to track which commits "exist" and add a test that
  `commit_exists(known_sha)` returns true and `commit_exists(unknown_sha)`
  returns false. (This locks the trait shape before we wire it up to
  sqlite.) Run: should fail with "no method named `commit_exists`".
- [x] **Step 2: Add the trait method to `Storage`.** Signature:
  `async fn commit_exists(&self, sha: &str) -> Result<bool>;`. Doc
  comment should reference plan-9 / RFC for context. Run the test:
  should now fail with "MockStorage doesn't implement
  commit_exists".
- [x] **Step 3: Implement on the test mock.** Whatever shape the
  existing mock uses for tracking commit state, mirror it. Test
  passes.
- [x] **Step 4: Commit.** `feat(core): add Storage::commit_exists for
  resume skip-check`.

### Task 1.2 — Sqlite impl

**Files:**
- Modify: `crates/ohara-storage/src/tables/commit.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [x] **Step 1: Write the failing test in
  `crates/ohara-storage/src/tables/commit.rs`** (alongside existing
  `commit::put` tests): insert two commits, assert
  `commit_exists` for both shas is true; assert
  `commit_exists` for a random sha is false. Run: should fail with
  "no method named `commit_exists`". *(Landed in
  `storage_impl.rs::tests` instead — that file owns every existing
  storage round-trip test; no inline `#[cfg(test)]` mod existed in
  any `tables/*.rs`.)*
- [x] **Step 2: Implement `pub fn commit_exists(c: &mut Connection,
  sha: &str) -> Result<bool>`** in `commit.rs`. Body:
  `SELECT 1 FROM commit_record WHERE sha = ?1 LIMIT 1`, return
  `Ok(rows.next()?.is_some())`. Run the table-level test: passes.
- [x] **Step 3: Wire up `SqliteStorage::commit_exists`** in
  `crates/ohara-storage/src/lib.rs` to delegate to
  `commit::commit_exists` inside `with_connection`. Cargo build
  workspace. Existing trait test from Task 1.1 should now pass
  end-to-end if a sqlite backing is used by the test (or stay
  scoped to mock — that's fine).
- [x] **Step 4: Commit.** `feat(storage): implement commit_exists
  with PK lookup`.

## Phase 2 — Indexer skip-check

### Task 2.1 — Per-commit short-circuit

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [x] **Step 1: Write the failing regression test.** In
  `indexer.rs`'s test module: build a `MockStorage` pre-seeded with
  two commits' worth of `commit_record` rows; run `Indexer::run` on
  a `MockGit` that returns those two SHAs plus one new SHA from
  `list_commits`; assert the embedder mock saw exactly **one**
  `embed_batch` call (for the new commit only). Run: should fail
  because today the indexer hits the embedder for all three.
- [x] **Step 2: Add the short-circuit.** In the per-commit loop in
  `Indexer::run`, before any hunk-extraction or embedding work,
  call `self.storage.commit_exists(&meta.commit_sha).await?` — if
  true, `continue`. Tracing: `tracing::debug!(sha = %meta.commit_sha,
  "skip already-indexed commit");`. Run the test: passes.
- [x] **Step 3: Verify watermark semantics.** Add a second test
  asserting that the watermark advances even when the loop has
  consecutive skipped commits (so a Ctrl-C right after a series of
  skips doesn't re-walk them next time). The fix here may be
  threading `latest_sha` through the skip branch — confirm before
  writing. Run: passes. *(Threaded `latest_sha` + `commits_done` +
  the periodic watermark-flush check through the skip branch so the
  invariant holds across long all-skip stretches.)*
- [x] **Step 4: Commit.** `feat(core): skip already-indexed commits
  on resume to avoid duplicate embedding`.

## Phase 3 — Doc + release

### Task 3.1 — Architecture doc note

**Files:**
- Modify: `docs-book/src/architecture/indexing.md`

- [x] **Step 1: Add a one-paragraph note** under the existing
  resume / abort-resume section: "The watermark is a single SHA;
  on resume the indexer also short-circuits per-commit when
  `commit_record` already has a row, so merge-heavy histories
  don't pay re-embedding cost for commits reachable via a
  non-watermark-ancestor path." Link to the RFC.
- [x] **Step 2: Commit.** `docs(arch): note skip-already-indexed on
  resume`.

### Task 3.2 — Changelog + version bump + release

**Files:**
- Modify: `docs-book/src/changelog.md`
- Modify: `Cargo.toml` (workspace version)

- [x] **Step 1: Add v0.6.3 changelog entry** above v0.6.2:
  resume now skips commits that already have a `commit_record` row,
  fixing duplicate embedding cost on merge-heavy repos. Link to
  the RFC.
- [x] **Step 2: Bump workspace version to `0.6.3`.**
- [ ] **Step 3: Tag and push.** `git tag -a v0.6.3 -m "Release
  v0.6.3: skip already-indexed commits on resume" && git push
  origin v0.6.3`. *(Deferred to user — release action.)*
- [ ] **Step 4: Watch the cargo-dist release workflow.** Confirm
  v0.6.3 artifacts publish; the per-host CoreML wiring from v0.6.2
  carries forward unchanged. *(Deferred to user.)*
- [ ] **Step 5: Spot-check on a live resume.** On a host that has
  the QuestDB index-in-progress: kill, `ohara update`, resume.
  Watch the log for `skip already-indexed commit` debug lines and
  confirm the first ~273 commits-already-in-DB get skipped under
  30s instead of taking ~14 min to re-embed. *(Deferred to user —
  needs the live host.)*

## Risks

1. **Skip path forgets to advance watermark.** Mitigated by the
   Task 2.1 Step 3 test. If watermark doesn't advance through
   skipped commits, an interrupted resume re-walks them — wasted
   work but still correct. Lower-severity than the original bug.
2. **`commit_exists` PK lookup adds cost on cold-index.** Sub-
   millisecond per commit; on a 5,800-commit run that's ~6 s of
   added wallclock against ~6 hours total. Acceptable.
3. **Test mock drift.** The existing `MockStorage` in `indexer.rs`
   needs the new method; if the mock's storage representation
   doesn't track `commit_record` natively, Task 1.1 Step 3 needs a
   small extension (e.g. a `HashSet<String>` of seen SHAs).
   Reversible.

## Out of scope (deferred)

- **Generation-number watermark.** A real fix for shallow clones
  and history rewrites; tracked as a v0.7 candidate.
- **Skip-stale-embedding-model detection.** When the embedding
  model changes, all `commit_record` rows are stale at the vector
  level. Today users handle this via `--force`; a smarter
  per-commit comparison is a separate plan.
- **`ohara status` reporting "X / Y commits indexed".** UX
  follow-up; data already exists in `repo` + `commit_record`.
