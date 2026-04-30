# Task 6: schema migration — refactor backlog

Captured at HEAD `bf5bf42`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers.

---

### 1. Empty `migrations/` dir lost at the red commit

- **Severity:** Low
- **Location:** `crates/ohara-storage/migrations/` (commit `bfd1b54`)
- **What:** The TDD red commit only contains `src/migrations.rs`; the empty
  `migrations/` directory it references can't be tracked by git, so anyone
  bisecting through `bfd1b54` hits a `refinery::embed_migrations!` proc-macro
  panic ("path not found") instead of the documented test failure.
- **Why:** Bisect / blame walks become misleading; the recorded "red" state
  doesn't actually reproduce.
- **Suggestion:** Add `crates/ohara-storage/migrations/.gitkeep` (or a
  `.gitignore` with `!.gitkeep`) the next time the directory is touched, and
  note this convention in `migrations.rs`.
- **Effort:** XS

### 2. `cargo clean` required between red and green refinery runs

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/migrations.rs:4-6`
- **What:** `refinery::embed_migrations!("migrations")` is a proc-macro that
  reads the directory at compile time; rustc/cargo cache the expansion, so
  adding `V1__initial.sql` after the red run does not invalidate the build
  and the test still fails until `cargo clean -p ohara-storage`.
- **Why:** Future migration authors will hit the same trap. Costs a confusing
  10–20 minutes the first time.
- **Suggestion:** Add a 2-line comment above the `embed_migrations!` call
  documenting the cache behavior and the `cargo clean -p ohara-storage`
  workaround when adding/removing migration files.
- **Effort:** XS

### 3. Migration test only spot-checks two table names

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/migrations.rs:18-43`
- **What:** `migrations_apply_to_fresh_db` asserts that `hunk` and `vec_hunk`
  exist but doesn't validate FK wiring, indexes, vec virtual-table dims,
  or the FTS5 tables. With `foreign_keys=ON` already applied, a row-insert
  smoke test per regular table would catch FK-direction bugs and column
  drift in one shot.
- **Why:** Tasks 7+ will add CRUD that depends on `foreign_keys=ON` actually
  enforcing the references declared in `V1__initial.sql`. Cheaper to assert
  the contract here than to debug a CRUD failure later.
- **Suggestion:** Extend the test to (a) insert into each non-virtual table
  with a satisfied FK, (b) assert an FK violation when the parent row is
  missing, (c) assert each `vec_*` table accepts a 384-dim `f32` blob.
- **Effort:** S

### 4. `foreign_keys=ON` is a load-bearing precondition with no doc

- **Severity:** Medium
- **Location:** `crates/ohara-storage/migrations/V1__initial.sql:32,45,46`
  and `crates/ohara-storage/src/pool.rs:65`
- **What:** Three `REFERENCES` clauses (`symbol.file_path_id`,
  `hunk.commit_sha`, `hunk.file_path_id`) are silently ignored unless
  `PRAGMA foreign_keys=ON` is set per connection. Today the pragma is set
  by `apply_pragmas`, but any future code path that opens a `Connection`
  outside `SqlitePoolBuilder` (tests, CLI tools, ad-hoc scripts) will get
  silent FK non-enforcement.
- **Why:** Task 7 will start writing rows that depend on FKs catching bugs.
  A connection opened the wrong way looks fine but corrupts the index.
- **Suggestion:** Add a header comment at the top of `V1__initial.sql`
  stating the FKs require `PRAGMA foreign_keys=ON` per connection, and
  add a doc comment on `migrations::run` reminding callers that pragmas
  must be applied before invoking it.
- **Effort:** XS

### 5. `mod embedded { embed_migrations!(...) }` wrapper is folkloric

- **Severity:** Low
- **Location:** `crates/ohara-storage/src/migrations.rs:4-6`
- **What:** Wrapping `refinery::embed_migrations!` in a private `embedded`
  module is a workaround for older refinery versions that polluted the
  caller's namespace. Refinery 0.8 may expose a cleaner public API
  (`embed_migrations!` returning a value, or `Runner` direct construction)
  that removes the need for the wrapper module.
- **Why:** Cargo-culting a workaround we may no longer need; one fewer
  layer of indirection improves readability.
- **Suggestion:** Confirm the current refinery 0.8 idiom; if the wrapper
  module is no longer required, inline the macro at module scope.
- **Effort:** XS

### 6. Plan-spec drift: `commit` renamed to `commit_record` without note

- **Severity:** Low
- **Location:** `crates/ohara-storage/migrations/V1__initial.sql:12`
- **What:** Spec §5 names the table `commit`; the migration uses
  `commit_record` (presumably to avoid the SQL-keyword footgun). The rename
  is sensible but is not documented anywhere, and the spec, plan tables in
  §13/§14, and future code reviewers will all reference `commit`.
- **Why:** Drift between spec terminology and table identifier means every
  future reader has to reconcile the names manually; cheap to fix once.
- **Suggestion:** Add a comment in `V1__initial.sql` ("renamed from spec
  `commit` to avoid SQLite reserved-word collisions") and update the spec
  §5 snippet (or a footnote) to match. Not blocking, but do this before
  Task 7 code is written against it.
- **Effort:** XS

### 7. Plan-spec drift: `blob_cache` PK and missing `symbols_json`

- **Severity:** Low
- **Location:** `crates/ohara-storage/migrations/V1__initial.sql:53-58`
- **What:** Spec §5 declares `blob_cache (blob_sha PRIMARY KEY, symbols_json
  TEXT, embedding_model TEXT, embedded_at INTEGER)`. The migration uses a
  composite PK `(blob_sha, embedding_model)` and drops `symbols_json`. The
  composite PK is arguably better (lets one blob be cached against multiple
  models) but is an undocumented divergence; the missing `symbols_json` will
  surface when symbol-extraction caching lands.
- **Why:** Either decision is defensible; what's not OK is silent drift.
- **Suggestion:** Either (a) reconcile the spec to match the schema and note
  the rationale for the composite PK, or (b) restore `symbols_json` if/when
  symbol-extraction caching needs it. Capture the decision in a comment at
  the table definition.
- **Effort:** XS

### 8. Plan-spec drift: `repo.indexed_at` added; symbol/hunk/commit `*_emb`
       columns dropped

- **Severity:** Low
- **Location:** `crates/ohara-storage/migrations/V1__initial.sql:3-10,12-19,30-40,43-49`
- **What:** Schema adds `repo.indexed_at TEXT` not in spec §5, and drops the
  `message_emb`, `diff_emb`, `source_emb` BLOB columns the spec lists on
  `commit`/`hunk`/`symbol`. Dropping them is correct (the `vec0` virtual
  tables already carry the embeddings), but the spec still shows the dual
  representation and will mislead readers.
- **Why:** Cheap to update the spec to reflect the chosen "embeddings live
  only in `vec_*` tables" model and avoid future readers asking "where do
  I write the embedding column".
- **Suggestion:** Update spec §5 to remove the BLOB columns and document
  `indexed_at`; or, if dual storage is still desired post-Plan-1, restore
  the columns. One paragraph either way.
- **Effort:** XS

---

### See also

`cargo clippy -p ohara-storage --all-targets` is clean at HEAD. Adjacent
warnings exist in `ohara-core` (unused imports / dead fields in `indexer.rs`
and `retriever.rs`) but those belong to Tasks 3–4's backlog, not this one.
