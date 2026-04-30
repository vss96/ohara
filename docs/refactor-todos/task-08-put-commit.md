# Task 8: put_commit — refactor backlog

Captured at HEAD `b851813`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in Task
6 (`commit_record` rename, schema drift) and Task 7 (`with_conn`,
`anyhow`-vs-`OhraError` boundary) are not duplicated here.

---

### 1. `commit::put` issues 3 INSERTs without a transaction

- **Severity:** High
- **Location:** `crates/ohara-storage/src/commit.rs:5-28`
- **What:** `put` writes `commit_record`, `vec_commit`, and `fts_commit` as
  three independent `c.execute(...)` calls. A panic, deadpool drop, or
  process kill between statements leaves the index in a torn state: a row
  in `commit_record` with no embedding (or no FTS entry, etc.). `INSERT OR
  REPLACE` masks the duplicate but not the missing-sibling case.
- **Why:** Spec §6 ("one transaction per N commits") and §13 ("indexing
  crashes mid-batch ⇒ watermark not advanced ⇒ reindex from last committed
  transaction; idempotent") presume each commit's three rows land or roll
  back together. Today they don't — and Task 9's `put_hunks` will inherit
  this shape if the pattern isn't fixed first.
- **Suggestion:** Wrap the three executes in `c.transaction()?` (rusqlite)
  or `c.unchecked_transaction()?`, commit at the end. Same change in
  `put_hunks` when it lands. Land before Task 9 to avoid retrofitting.
- **Effort:** XS

### 2. `vec_to_bytes` / `bytes_to_vec` belong in a shared codec module

- **Severity:** High
- **Location:** `crates/ohara-storage/src/commit.rs:30-42`
- **What:** Both helpers are `pub` in `commit.rs`, but Task 9's `hunk.rs`
  will need `vec_to_bytes` for `vec_hunk` writes and Task 14's KNN read
  path will need `bytes_to_vec`. Importing them as
  `crate::commit::vec_to_bytes` from a sibling `hunk` module is a layering
  smell (hunk has no business depending on commit).
- **Why:** Pre-emptive: extracting now is one file move; doing it after
  Task 9 lands means updating two call sites and a test. Codec also has no
  domain-specific behavior — it's pure little-endian f32 packing.
- **Suggestion:** Move both fns to a new `crates/ohara-storage/src/vec_codec.rs`
  (or `byte_codec.rs`) and re-export, or just `pub(crate) use`. Land before
  Task 9 starts.
- **Effort:** XS

### 3. No round-trip unit test for `vec_to_bytes` / `bytes_to_vec`

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/commit.rs:30-42`
- **What:** Neither helper has a unit test. The integration test in
  `storage_impl.rs` only asserts `count(*) = 1` on `vec_commit` — it never
  reads the blob back, so a byte-order bug, off-by-one in
  `Vec::with_capacity`, or `chunks_exact` truncation would all pass.
- **Why:** These are pure functions on an easily-fuzzable signature. A
  `#[test] fn round_trip()` for an empty vec, a single f32, and a 384-dim
  vec is ~10 lines and covers every realistic input shape.
- **Suggestion:** Add `#[cfg(test)] mod tests` to `commit.rs` (or to the
  new `vec_codec.rs` per item #2) with `assert_eq!(bytes_to_vec(&vec_to_bytes(&v)), v)`
  for `[]`, `[1.0]`, and `vec![0.1; 384]`.
- **Effort:** XS

### 4. `bytes_to_vec` silently truncates non-multiple-of-4 inputs

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/commit.rs:36-42`
- **What:** `b.chunks_exact(4)` drops a trailing 1–3 byte remainder
  silently. If a `vec_commit` row is ever corrupt (truncated WAL,
  hardware error, schema-version mismatch where dim changes), the loader
  returns a shorter `Vec<f32>` than expected with no signal.
- **Why:** A short-by-N embedding feeding into KNN gives wrong-but-plausible
  results — the worst failure mode. Better to fail loud at decode time
  than to debug ranking drift weeks later.
- **Suggestion:** Either (a) `debug_assert_eq!(b.len() % 4, 0)` plus a
  comment that the schema guarantees alignment, or (b) return
  `Result<Vec<f32>>` and bubble a decode error. (a) is cheaper given the
  schema's `FLOAT[384]` constraint; (b) is more defensive.
- **Effort:** XS

### 5. Test asserts row counts only — no embedding round-trip, no FTS search

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/storage_impl.rs:94-121`
- **What:** `put_commit_persists_meta_and_embedding` checks `count(*) = 1`
  on `commit_record` and `vec_commit`. It does *not* (a) read the
  embedding back and compare to input, (b) verify `fts_commit` is searchable
  via `MATCH`, or (c) confirm `commit_record` columns (parent_sha, ts,
  author, is_merge) round-trip. The test name ("persists meta and embedding")
  oversells what's checked.
- **Why:** Three of the four behaviors `put` is responsible for are
  unverified. A regression in any of them (wrong column order, dropped
  `is_merge` cast, FTS table named differently) passes today.
- **Suggestion:** Extend the test (or split into three) to (a) `SELECT
  message_emb FROM vec_commit` and `bytes_to_vec` it back, asserting
  equality, (b) `SELECT sha FROM fts_commit WHERE message MATCH 'first'`
  returns `"abc"`, (c) `SELECT * FROM commit_record WHERE sha = 'abc'`
  matches the input `CommitMeta`.
- **Effort:** S

### 6. `put_commit` ignores `_repo_id` entirely

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/storage_impl.rs:64` and
  `crates/ohara-storage/src/commit.rs:5`
- **What:** Trait takes `repo_id: &RepoId` but the impl prefixes it with `_`
  and never uses it. `commit_record` has no `repo_id` column; the same SHA
  from two different repos would collide on the `sha PRIMARY KEY` and
  `INSERT OR REPLACE` would silently overwrite the first repo's row.
- **Why:** Today single-repo-only by accident. Either the schema needs
  `repo_id` on `commit_record`/`hunk`/`symbol`, or the trait shouldn't
  take it, or the single-repo-per-DB assumption needs documenting.
- **Suggestion:** Pick one: (a) drop `repo_id` from the trait until
  multi-repo lands, (b) add `repo_id TEXT NOT NULL REFERENCES repo(id)`
  in a V2 migration, or (c) document the single-repo-per-DB assumption.
  Decide before Task 14 (retrieval) cements the assumption.
- **Effort:** S (option c) / M (option b)

### 7. No module-level doc on `commit.rs`

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/commit.rs:1`
- **What:** `commit.rs` introduces a non-obvious 3-table fan-out
  (`commit_record`, `vec_commit`, `fts_commit`) and byte-codec helpers
  with zero context for readers.
- **Why:** Task 9's `hunk.rs` author will follow this pattern; a 2-line
  `//!` explaining the fan-out and the `INSERT OR REPLACE` idempotency
  choice saves them a re-derivation.
- **Suggestion:** Add a `//!` summary; update once item #1 lands to
  mention the transaction.
- **Effort:** XS

### 8. `record.clone()` in `put_commit` foreshadows a hot-path allocation

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:65`
- **What:** `with_conn`'s closure must be `'static + Send`, so the
  borrowed `&CommitRecord` can't cross the `interact` boundary. The clone
  copies a 384-`f32` (~1.5 KiB) `Vec` plus the message per call. At ~512
  commits per batch (spec §6) that's ~768 KiB avoidable per batch.
- **Why:** Not a correctness issue, but `put_hunks(&[HunkRecord])` clones
  a whole slice — measurable at scale. Worth flagging so Task 9 doesn't
  bake in the same shape.
- **Suggestion:** For Task 9, move the `Vec<HunkRecord>` into the closure
  instead of cloning per record. Larger refactor (owned records on the
  trait) can wait.
- **Effort:** XS (note) / M (refactor)

---

### See also

- `cargo clippy -p ohara-storage --all-targets` is clean at HEAD. Pre-existing
  `ohara-core` warnings (`indexer.rs`, `retriever.rs`) belong to Tasks 3–4's
  backlog, unchanged.
- `bytes_to_vec` is currently unused at HEAD; it's intentional Task 9 / Task 14
  scope (KNN read path), so leave it as-is. Items #2–#4 still apply.
- The `commit_record` table name (vs. spec's `commit`) is tracked in
  Task 6 backlog and not duplicated here.
- Items #1 and #2 are time-sensitive: addressing them before Task 9 lands
  avoids a retrofit. Items #3–#5 and #7 can land any time; items #6 and #8
  are cross-cutting and should be discussed before Task 14.
