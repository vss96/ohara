# Task 7: repo CRUD — refactor backlog

Captured at HEAD `75c90b0`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. The six `unimplemented!()`
stubs in `storage_impl.rs` are explicit Task 8/9 scope and are not refactor
candidates yet — but the *plumbing pattern* around them is.

---

### 1. Triple `map_err` chain will be replicated 6 more times in Tasks 8–9

- **Severity:** High
- **Location:** `crates/ohara-storage/src/storage_impl.rs:31-40,43-51,54-63`
- **What:** Every `Storage` trait method follows the same shape: `pool.get()
  .await.map_err(...)?.interact(move |c| ...).await.map_err(...)?
  .map_err(...)`. Three identical `OhraError::Storage(e.to_string())` calls
  per method × 3 methods today + 6 Task 8/9 stubs ≈ 27 copies of the same
  boilerplate if left as-is.
- **Why:** Each new method is a copy-paste vector for subtle bugs (wrong
  error variant, missing `move`, dropped `?`), and the signal-to-noise ratio
  in the trait impl tanks. Cheaper to extract once now, before Tasks 8–9
  cement nine copies.
- **Suggestion:** Add a private helper `async fn with_conn<F, R>(&self, f: F)
  -> CoreResult<R>` on `SqliteStorage` that collapses each trait method to
  one line. Land before Task 8 starts to avoid retrofitting six new methods.
- **Effort:** S

### 2. `SqliteStorage::open` returns `anyhow::Result`, trait methods return `ohara_core::Result`

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/storage_impl.rs:17,29`
- **What:** The constructor leaks `anyhow::Result<Self>` while every trait
  method maps into `ohara_core::Result<()>` / `OhraError::Storage`. Callers
  juggle two error types in the same code path.
- **Why:** `bin/` or service wiring must pull in `anyhow` purely for
  `open` or write their own conversion. The same mapping is cheap and
  aligns the constructor with the rest of the impl.
- **Suggestion:** Change `open()` to return `ohara_core::Result<Self>` with
  the same `e.to_string() -> OhraError::Storage` mapping. Keep `anyhow`
  internal if extra context is desired, convert at the boundary.
- **Effort:** XS

### 3. `Utc::now()` baked into `set_watermark` blocks deterministic testing

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/repo.rs:33`
- **What:** `set_watermark` calls `Utc::now().to_rfc3339()` directly. Tests
  asserting on `indexed_at` must read-back-and-compare-loosely or mock the
  global clock; multiple watermark advances inside one batch can't share a
  single timestamp.
- **Why:** Today's test skips asserting on `indexed_at`; the next test that
  needs to ("watermark write advances `indexed_at`") will hit this. Pure
  functions with the time injected are dramatically easier to test.
- **Suggestion:** Push `Utc::now()` to the trait boundary —
  `set_watermark(c, id, sha, now: DateTime<Utc>)` and have
  `SqliteStorage::set_last_indexed_commit` produce the timestamp.
- **Effort:** XS

### 4. `upsert` `ON CONFLICT` silently keeps stale `first_commit_sha`

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/repo.rs:7-13`
- **What:** The upsert updates only `path` on conflict, so a second call with
  a different `first_commit_sha` for the same `repo_id` is silently accepted
  and the stored value diverges from the argument. Today the `RepoId` is
  derived from `hash(first_commit_sha + path)`, so a mismatch implies caller
  bug, not a legitimate update — but the code accepts it without complaint.
- **Why:** Failing loudly on inconsistent identity is cheaper than
  debugging downstream lineage corruption. Either the args are redundant
  with `repo_id` (and the upsert can drop them on conflict) or they're
  authoritative (and a mismatch should error).
- **Suggestion:** Either (a) on conflict, assert `first_commit_sha` matches
  the stored value and return `OhraError::Storage` if not, or (b) document
  that `first_commit_sha` is informational-only post-creation and add a
  comment to the SQL explaining why it's not in the `DO UPDATE SET` list.
- **Effort:** XS

### 5. `get_status` hardcodes `commits_behind_head: 0` with a TODO-shaped comment

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/repo.rs:25`
- **What:** Storage returns `commits_behind_head: 0` always and defers
  computation to "the caller … from git rev-list". Every caller of
  `get_index_status` now must know one field is unfilled, and any caller
  that forgets quietly surfaces "0" to users.
- **Why:** Either the field belongs in `IndexStatus` (and someone owns
  filling it — likely the indexer/orchestrator) or it doesn't (and the type
  should split into a storage-level `IndexedState` + a computed `IndexLag`).
  Current shape invites bugs.
- **Suggestion:** When orchestrator wiring lands (Task 10+), move the field
  into a wrapper struct populated by the orchestrator, or document on the
  trait method that the field is "always 0 from Storage; populated by the
  caller".
- **Effort:** S

### 6. Single test conflates three methods; failure attribution is muddy

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:84-97`
- **What:** `open_repo_round_trip` exercises three trait methods in one
  test; any single failure fails the whole test and assertion ordering
  masks whether the fault is in the upsert path or the read path.
- **Why:** Tasks 8–9 add ~6 more methods needing tests; landing a
  convention now ("one test per method + one end-to-end happy-path") keeps
  the file legible past 1k lines.
- **Suggestion:** Split into `open_repo_inserts_row`,
  `get_index_status_for_unknown_repo_returns_empty`,
  `get_index_status_after_open`, and
  `set_last_indexed_commit_advances_watermark` (the last asserts
  `indexed_at` once #3 lands). Keep one round-trip test.
- **Effort:** XS

### 7. Clippy: `redundant_closure` on `migrations::run`

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:21`
- **What:** `conn.interact(|c| migrations::run(c))` triggers
  `clippy::redundant_closure`; can be replaced with
  `conn.interact(migrations::run)`. Sole new clippy warning introduced by
  this task.
- **Why:** Cheap, mechanical, keeps `cargo clippy -p ohara-storage` clean
  (matching Task 6's clean baseline).
- **Suggestion:** Apply the suggested fix verbatim.
- **Effort:** XS

### 8. `schema_version = 1` hardcoded in `repo` upsert duplicates migration state

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/repo.rs:9`
- **What:** The upsert hardcodes `schema_version = 1`. The authoritative
  version lives in `refinery_schema_history`. Two sources of truth: a future
  V2 migration must either update existing `repo.schema_version` rows or
  the per-repo column drifts from reality.
- **Why:** Spec §5 ("version stamped in `repo.schema_version`; mismatch
  requires `ohara reindex`") suggests this is the per-repo migration stamp,
  not the DB's. Either way, the constant `1` literal is wrong.
- **Suggestion:** Either drop `repo.schema_version` (use refinery's table),
  or compute via `MAX(version)` from `refinery_schema_history` at write
  time. Decide before V2 lands.
- **Effort:** S

---

### See also

- `cargo clippy -p ohara-storage --all-targets` is clean apart from item #7.
  Pre-existing `ohara-core` warnings (`indexer.rs`, `retriever.rs`) belong to
  Tasks 3–4's backlog and are unchanged by this task.
- Items #1 and #6 are time-sensitive: addressing them before Task 8 lands
  avoids a 6× retrofit. Items #2–#5 and #8 can wait until after Task 9.
- The six `unimplemented!()` stubs in `storage_impl.rs` are intentional Task
  8/9 scope and are not tracked here.
