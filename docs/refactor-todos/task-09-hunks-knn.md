# Task 9: hunks, knn, blob_cache — refactor backlog

Captured at HEAD `c18b462`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in Task
6 (schema drift, FK precondition), Task 7 (`with_conn`, `anyhow`-vs-
`OhraError` boundary), and Task 8 (transactional `commit::put`,
`vec_codec` extraction) are not duplicated here.

---

### 1. `similarity = -distance` silently breaks `Retriever::rank_hits`

- **Severity:** High
- **Location:** `crates/ohara-storage/src/hunk.rs:90-92`
- **What:** Plan asked for `similarity = (1.0 - distance).clamp(0, 1)`; the
  implementer deviated to `-distance` because L2 distances on un-normalised
  384-dim embeddings routinely exceed 1.0 and would clamp to 0. The KNN
  test passes (asserts ordering only), but Task 4's `rank_hits` computes
  `0.7 * similarity + 0.2 * recency + 0.1 * msg_sim` with `recency, msg_sim
  ∈ [0,1]`. With `similarity ∈ (-∞, 0]`, `combined_score` goes negative
  and unbounded. Sort still works (monotonic), but the number surfaced in
  `PatternHit` is meaningless and violates spec §7 (`similarity: f32, //
  0..1`).
- **Why:** Must be fixed before Task 14 wires `find_pattern` end-to-end.
- **Suggestion:** Switch `hunk::knn` to sqlite-vec's `vec_distance_cosine`
  (or L2-normalise embeddings at write-time) and compute
  `similarity = (1.0 - distance).clamp(0.0, 1.0)`. Add a `rank_hits` test
  asserting `combined_score ∈ [0, 1]` for in-range inputs. Alternative:
  move the conversion into `Retriever` and document `HunkHit::similarity`
  as raw `-distance` — but spec drift remains.
- **Effort:** S

### 2. `str_to_change_kind` silently maps unknown values to `Modified`

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/hunk.rs:138-145`
- **What:** A bad string in `hunk.change_kind` (corruption, schema drift, a
  future "copied" variant) is silently coerced to `ChangeKind::Modified`.
  The reverse `change_kind_to_str` is total, so the only way to get an
  unknown string is corruption — but then we want to know.
- **Why:** Same failure-mode argument as Task 8 #4 (`bytes_to_vec` truncation):
  a wrong-but-plausible enum value pollutes ranking and any future symbol
  attribution without any signal.
- **Suggestion:** Return `Result<ChangeKind>` (or at least
  `Option<ChangeKind>`) and bubble a decode error from `knn`. Alternative:
  `debug_assert!` plus a comment that the schema's CHECK constraint
  guarantees the four values, then `unreachable!` for the default arm.
- **Effort:** XS

### 3. `upsert_file_path` does INSERT-then-SELECT (N+1 round-trip)

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/hunk.rs:124-136`
- **What:** Each hunk insert makes two SQL statements: an `INSERT ... ON
  CONFLICT DO UPDATE` followed by a `SELECT id FROM file_path WHERE path =
  ?1`. SQLite 3.35+ supports `RETURNING`, which collapses both into one
  statement and removes the redundant index probe.
- **Why:** Indexing 100k commits × ~5 hunks/commit = 500k file-path lookups.
  Halving the round-trip count is meaningful at scale, and the rewrite is
  small.
- **Suggestion:** `INSERT ... ON CONFLICT(path) DO UPDATE SET language =
  COALESCE(excluded.language, file_path.language) RETURNING id`, then
  `query_row` returns the id directly.
- **Effort:** XS

### 4. No module-level docs on `hunk.rs` or `blob_cache.rs`

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/hunk.rs:1`,
  `crates/ohara-storage/src/blob_cache.rs:1`
- **What:** `hunk.rs` introduces a 3-table fan-out (`file_path`, `hunk`,
  `vec_hunk`) plus a non-obvious `MATCH` + `k = :k` sqlite-vec calling
  convention. `blob_cache.rs` is small but its `(blob_sha, model)` PK
  semantics (re-embed on model change) deserves a one-liner. Same gap
  Task 8 backlog #7 flagged for `commit.rs`.
- **Why:** Plan 2's symbol-attribution module will follow this shape; a
  short `//!` saves the next author a re-derivation of why `MATCH` and
  `k =` both appear in the WHERE clause.
- **Suggestion:** Add a `//!` block to each: hunk.rs explains the 3-table
  fan-out, the sqlite-vec MATCH+k idiom, and the `INSERT INTO hunk` (no
  `OR REPLACE`) idempotency choice; blob_cache.rs explains the composite
  PK and "model change ⇒ re-embed" semantics.
- **Effort:** XS

### 5. `knn` builds SQL via `format!` with conditional fragments

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/hunk.rs:39-58`
- **What:** Two optional filters (`language`, `since_unix`) spliced via
  `format!`; the `Box<dyn ToSql>` binds Vec is already a workaround for
  `query_map`'s slice signature. Plan 2 will likely add `path_prefix`,
  `author`, `change_kind`, and possibly `repo_id` (Task 8 #6 multi-repo).
  Each filter doubles the implicit shape space.
- **Why:** Pre-emptive. Not worth doing for two filters; worth flagging
  before more arrive.
- **Suggestion:** Defer until a third filter lands or Task 14 asks for
  `path_prefix`. Then evaluate sea-query vs a 30-line builder helper.
- **Effort:** M (when triggered)

### 6. `put_many` clones `Vec<HunkRecord>` to cross the `interact` boundary

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:68-71`
- **What:** Same shape as Task 8 #8: `with_conn`'s closure must be
  `'static + Send`, so `&[HunkRecord]` is cloned per call. ~5 hunks/commit
  × 512 commits/batch × 1.5 KiB embedding ≈ ~3.8 MiB cloned per batch.
- **Why:** Cross-cutting. A single trait-shape fix (owned records, or
  `Cow<[HunkRecord]>`) resolves Task 8 #8 and this together.
- **Suggestion:** Bundle with Task 8 #8 — single PR.
- **Effort:** XS (note) / M (refactor)

### 7. KNN test uses uniform-value embeddings `[v; 384]`

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:238-254`
- **What:** Each test embedding is a constant vector (`[0.1; 384]`,
  `[0.5; 384]`, etc.). For L2 distance these collapse to a 1-D problem,
  so the test cannot detect bugs that only manifest on real embeddings:
  a flipped sign in `vec_to_bytes`, an off-by-one in `chunks_exact(4)`,
  or a dimension mismatch that happens to produce a sensible-looking
  ordering on uniform inputs.
- **Why:** Cheap to fix; raises the floor for KNN regressions.
- **Suggestion:** Generate embeddings as
  `(0..384).map(|i| (i as f32 + offset) * 0.001).collect()` (or any
  per-dim variation) so a byte-order or stride bug fails the ordering
  assertion. Keep `[0.0; 384]` only for tests that don't exercise
  distance.
- **Effort:** XS

### 8. No FK enforcement test for `hunk.commit_sha → commit_record.sha`

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/storage_impl.rs:202-230`
- **What:** Task 6 #4 flagged that `foreign_keys=ON` has no test. Task 9
  is the first user of cross-table FKs (`hunk.commit_sha`,
  `hunk.file_path_id`, `vec_hunk.hunk_id`), so the cost is now real —
  inserting a hunk with a non-existent `commit_sha` should fail; nothing
  verifies it. Trigger for resolving Task 6 #4, not a new item.
- **Suggestion:** Add `put_hunks_rejects_unknown_commit` asserting an
  FK-violation error for `commit_sha = "nope"`.
- **Effort:** XS

---

### See also

- `cargo clippy -p ohara-storage --all-targets` is clean at HEAD;
  pre-existing `ohara-core` warnings belong to Tasks 3–4 backlog.
- `-distance` deviation and "similarity 0..1" spec drift are the same
  issue — captured once as #1.
- `put_head_symbols` no-op is task-scope (Plan 2), not refactor-scope.
- `put_many`'s transactional shape mirrors Task 8's `commit::put`
  (resolved `2828974`); no new item.
- Time-sensitive: #1 (before Task 14). Trigger-based: #5, #6. Anytime:
  #2, #3, #4, #7, #8.
