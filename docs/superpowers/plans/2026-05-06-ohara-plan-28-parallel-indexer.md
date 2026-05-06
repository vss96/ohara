# ohara plan-28 — Parallel commit pipeline (Spec D) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each
> green implementation.

**Goal:** Replace the coordinator's per-commit `for` loop with an
actor-style pipeline (walker task + N worker tasks + bounded mpsc
channel). Each commit gets a deterministic ULID derived from
`(commit_time, commit_sha)`; persistence is order-free; resume falls
back to plan-9's `commit_exists` skip. New `--workers <N>` CLI flag
defaults to `num_cpus::get()`; `--workers 1` reproduces today's serial
behaviour.

**Architecture:** Walker actor enumerates HEAD-reachable commits and
emits `(CommitMeta, Ulid)` to a `tokio::sync::mpsc::channel(N)`.
Worker pool consumes; each worker runs the full per-commit pipeline
end-to-end (`hunk_chunk → attribute → embed → persist`). SQLite WAL
serializes concurrent writes naturally; no persist serializer. A new
V6 migration adds a `commit.ulid` column + index. `ohara status`'s
`last_indexed_commit` becomes derived from `MAX(ulid)`.

**Tech stack:** Rust 2021, existing `git2` / `rusqlite` / `tokio` /
`clap`, plus `ulid = "1"` and `num_cpus = "1"` (added to workspace).

**Spec:** `docs/superpowers/specs/2026-05-06-ohara-parallel-indexer-design.md`.

**Sequencing:** Phase A (ULID + V6) and Phase B (status query) are
independent. Phase C (actor pipeline) depends on A. Phase D (CLI) and
Phase E (perf+docs) depend on C.

---

## Phase A — ULID primitive + V6 migration

### Task A.1 — Add `ulid` and `num_cpus` to workspace deps

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/ohara-core/Cargo.toml`

- [ ] **Step 1: Add to `[workspace.dependencies]`**

In `Cargo.toml` workspace root, append (alphabetical position is fine):

```toml
ulid = "1"
num_cpus = "1"
```

- [ ] **Step 2: Reference from `ohara-core`**

In `crates/ohara-core/Cargo.toml` `[dependencies]`:

```toml
ulid = { workspace = true }
num_cpus = { workspace = true }
```

- [ ] **Step 3: Verify**

```
cargo build -p ohara-core
```
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/ohara-core/Cargo.toml
git commit -m "chore(core): add ulid + num_cpus workspace deps for plan-28"
```

---

### Task A.2 — `ulid_for_commit` in `ohara-core::types`

**Files:**
- Modify: `crates/ohara-core/src/types.rs`
- Modify: `crates/ohara-core/src/lib.rs` (re-export)

- [ ] **Step 1: Write failing tests**

Append to `crates/ohara-core/src/types.rs` (in or alongside an existing
test module):

```rust
#[cfg(test)]
mod ulid_for_commit_tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = ulid_for_commit(1_700_000_000, "deadbeef".repeat(5).as_str());
        let b = ulid_for_commit(1_700_000_000, "deadbeef".repeat(5).as_str());
        assert_eq!(a, b);
    }

    #[test]
    fn earlier_time_sorts_before_later() {
        let early = ulid_for_commit(1_000_000, "0".repeat(40).as_str());
        let late = ulid_for_commit(2_000_000, "0".repeat(40).as_str());
        assert!(early.to_string() < late.to_string());
    }

    #[test]
    fn different_shas_at_same_time_produce_different_ulids() {
        let a = ulid_for_commit(1_700_000_000, "a".repeat(40).as_str());
        let b = ulid_for_commit(1_700_000_000, "b".repeat(40).as_str());
        assert_ne!(a, b);
    }

    #[test]
    fn negative_time_is_clamped_to_zero() {
        // Some commits have author dates before the unix epoch; we
        // saturate to 0 rather than panicking.
        let u = ulid_for_commit(-1_000, "a".repeat(40).as_str());
        let _ = u; // any valid ULID is acceptable
    }
}
```

- [ ] **Step 2: Run to confirm fail**

```
cargo test -p ohara-core ulid_for_commit_tests
```
Expected: FAIL — `ulid_for_commit` undefined.

- [ ] **Step 3: Implement**

Add to `crates/ohara-core/src/types.rs` (above the test module):

```rust
/// Plan 28: derive a stable ULID from a commit's `(commit_time, sha)`.
/// Lexicographic sort = chronological sort. Deterministic.
///
/// 48-bit timestamp = `commit_time_seconds * 1000` ms.
/// 80-bit randomness slot = first 20 hex chars (10 bytes) of `sha`.
pub fn ulid_for_commit(commit_time_seconds: i64, sha: &str) -> ulid::Ulid {
    let ms = (commit_time_seconds.max(0) as u64).saturating_mul(1000);
    let mut rand_bytes = [0u8; 10];
    hex::decode_to_slice(&sha[..20], &mut rand_bytes)
        .expect("invariant: commit_sha is 40-hex");
    let mut rand_buf = [0u8; 16];
    rand_buf[6..].copy_from_slice(&rand_bytes);
    let rand_u128 = u128::from_be_bytes(rand_buf);
    ulid::Ulid::from_parts(ms, rand_u128)
}
```

- [ ] **Step 4: Re-export**

In `crates/ohara-core/src/lib.rs`, extend the existing types re-export
line to include `ulid_for_commit`:

```rust
pub use types::{
    AttributionKind, ChangeKind, CommitMeta, ContentHash, Hunk, HunkSymbol, Provenance,
    RepoId, Symbol, SymbolKind, ulid_for_commit,
};
```

- [ ] **Step 5: Verify**

```
cargo test -p ohara-core ulid_for_commit_tests
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/types.rs crates/ohara-core/src/lib.rs
git commit -m "feat(core): ulid_for_commit derives stable per-commit ULID"
```

---

### Task A.3 — V6 migration: `commit.ulid` column + index

**Files:**
- Create: `crates/ohara-storage/migrations/V6__commit_ulid.sql`

- [ ] **Step 1: Write the migration**

```sql
-- Plan 28: per-commit ULID for time-sortable, parallel-write-friendly
-- ordering. Pre-V6 rows get '' (empty default) and are excluded from
-- ULID-ordered reads (e.g. ohara status's MAX(ulid) query) until a
-- --rebuild repopulates them. New writes always include the ULID.
ALTER TABLE commit ADD COLUMN ulid TEXT NOT NULL DEFAULT '';
CREATE INDEX idx_commit_ulid ON commit (ulid);
```

- [ ] **Step 2: Verify**

```
cargo test -p ohara-storage
```
Expected: green. Refinery applies V6 on test storage open.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-storage/migrations/V6__commit_ulid.sql
git commit -m "feat(storage): V6 migration — commit.ulid column + index"
```

---

### Task A.4 — Extend `CommitRecord` with `ulid` and write through `put_commit`

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`
- Modify: `crates/ohara-storage/src/tables/commit.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs` (no behavior change
  expected — `put_commit` impl delegates to `tables::commit`)
- Modify: existing call sites that construct `CommitRecord` (likely
  `crates/ohara-core/src/indexer/stages/persist.rs`).

- [ ] **Step 1: Add the field**

In `crates/ohara-core/src/storage.rs`, change `CommitRecord`:

```rust
#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub meta: CommitMeta,
    pub message_emb: Vector,
    /// Plan 28: ULID derived via `ulid_for_commit(meta.ts, &meta.commit_sha)`.
    /// Stored alongside the commit row for time-sorted reads.
    pub ulid: String,
}
```

- [ ] **Step 2: Update the SQL insert in `tables::commit`**

Read `crates/ohara-storage/src/tables/commit.rs` to find the
`INSERT INTO commit (...)` statement. Add the `ulid` column to the
column list and the `?` placeholder list, binding `record.ulid.as_str()`.

Compile failures will tell you every call site that constructs
`CommitRecord` without the new field. Update each:

```rust
let record = CommitRecord {
    meta: commit_meta.clone(),
    message_emb: vec_for_message,
    ulid: ohara_core::ulid_for_commit(commit_meta.ts, &commit_meta.commit_sha).to_string(),
};
```

The most important call site is in
`crates/ohara-core/src/indexer/stages/persist.rs` where the
record is built before `storage.put_commit(...)`.

- [ ] **Step 3: Add a regression test**

In `crates/ohara-storage/src/tables/commit.rs` test module (or a new
one), add:

```rust
#[tokio::test]
async fn put_commit_writes_ulid_column() {
    use ohara_core::storage::CommitRecord;
    use ohara_core::types::{CommitMeta, RepoId};
    use ohara_core::ulid_for_commit;
    let storage = /* open a SqliteStorage on a tempfile, mirror existing pattern */;
    let repo_id = RepoId::from_components("/tmp/x", "0".repeat(40).as_str());
    storage.open_repo(&repo_id, "/tmp/x", &"0".repeat(40)).await.unwrap();

    let meta = CommitMeta {
        commit_sha: "deadbeef".repeat(5),
        parent_sha: None,
        is_merge: false,
        author: None,
        ts: 1_700_000_000,
        message: "hello".into(),
    };
    let ulid = ulid_for_commit(meta.ts, &meta.commit_sha).to_string();
    let record = CommitRecord {
        meta,
        message_emb: vec![0.0_f32; 4],
        ulid: ulid.clone(),
    };
    storage.put_commit(&repo_id, &record).await.unwrap();

    // Read back via the pool; expect ulid stored.
    let conn = storage.pool().get().await.unwrap();
    let stored: String = conn
        .interact(move |c| {
            c.query_row(
                "SELECT ulid FROM commit WHERE commit_sha = ?1",
                [&"deadbeef".repeat(5)],
                |r| r.get::<_, String>(0),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, ulid);
}
```

(Adapt the `/* open a SqliteStorage on a tempfile */` part to whatever
existing test scaffolding the file uses — see Plan-27 A.3 for the
canonical pattern.)

- [ ] **Step 4: Run**

```
cargo test -p ohara-core
cargo test -p ohara-storage
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

All green.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/storage.rs \
        crates/ohara-storage/src/tables/commit.rs \
        crates/ohara-core/src/indexer/stages/persist.rs
# also stage any other call sites updated for CommitRecord
git commit -m "feat(storage): persist commit.ulid via CommitRecord field"
```

---

## Phase B — `ohara status` derives `last_indexed_commit` from MAX(ulid)

### Task B.1 — `Storage::latest_indexed_by_ulid` trait method

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`
- Modify: `crates/ohara-storage/src/tables/commit.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [ ] **Step 1: Add trait method with default impl**

In `crates/ohara-core/src/storage.rs` `pub trait Storage`, append:

```rust
    /// Plan 28: return the commit with the highest ULID for this repo
    /// (i.e. the most recently committed of the indexed commits, by
    /// commit_time). Returns None when no commits are indexed or all
    /// rows have empty ULID (pre-V6).
    ///
    /// Default returns None — appropriate for in-memory test storages.
    /// SqliteStorage overrides with a real query.
    async fn latest_indexed_by_ulid(
        &self,
        repo_id: &RepoId,
    ) -> Result<Option<CommitMeta>> {
        let _ = repo_id;
        Ok(None)
    }
```

- [ ] **Step 2: Implement in `tables::commit`**

Add a function to `crates/ohara-storage/src/tables/commit.rs`:

```rust
pub fn latest_by_ulid(
    c: &Connection,
    repo_id: &str,
) -> Result<Option<ohara_core::types::CommitMeta>> {
    // ulid != '' excludes pre-V6 rows.
    let mut stmt = c.prepare(
        "SELECT commit_sha, parent_sha, is_merge, author, ts, message
         FROM commit
         WHERE repo_id = ?1 AND ulid != ''
         ORDER BY ulid DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row([repo_id], |r| {
            Ok(ohara_core::types::CommitMeta {
                commit_sha: r.get::<_, String>(0)?,
                parent_sha: r.get::<_, Option<String>>(1)?,
                is_merge: r.get::<_, bool>(2)?,
                author: r.get::<_, Option<String>>(3)?,
                ts: r.get::<_, i64>(4)?,
                message: r.get::<_, String>(5)?,
            })
        })
        .optional()?;
    Ok(row)
}
```

(`optional()` is from `rusqlite::OptionalExtension`; bring it into
scope at the top of the file if not already there.)

The exact column names on the existing `commit` table need to match —
read the V1 migration or `tables::commit` insert statements first to
confirm the column ordering.

- [ ] **Step 3: Wire into `SqliteStorage`**

In `crates/ohara-storage/src/storage_impl.rs`, override the trait
method to delegate via the project's existing connection pattern
(`with_conn` or `pool.get` + `interact`).

- [ ] **Step 4: Update `ohara status`**

In `crates/ohara-cli/src/commands/status.rs::run`, replace the line
that pulls `last_indexed_commit` from the `repo` table-cache with a
call to the new method, falling back to the existing field for
backwards compatibility:

```rust
    let derived = storage.latest_indexed_by_ulid(&repo_id).await?;
    let last_indexed = derived
        .map(|m| m.commit_sha)
        .or(st.last_indexed_commit.clone())
        .unwrap_or_else(|| "<none>".into());
```

(Adjust to the actual local-variable names in the existing code.)

- [ ] **Step 5: Run + commit**

```
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/storage.rs \
        crates/ohara-storage/src/tables/commit.rs \
        crates/ohara-storage/src/storage_impl.rs \
        crates/ohara-cli/src/commands/status.rs
git commit -m "feat(cli): ohara status derives last_indexed_commit from MAX(ulid)"
```

---

## Phase C — Actor pipeline

### Task C.1 — `Indexer::with_workers(n)` builder + thread to `Coordinator`

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs`

- [ ] **Step 1: Add field + builder to `Indexer`**

In `crates/ohara-core/src/indexer.rs`, add to the struct:

```rust
    /// Plan 28: number of worker tasks for the actor-style commit
    /// pipeline. Defaults to num_cpus::get() at builder time.
    workers: usize,
```

In `Indexer::new`, default it:

```rust
            workers: num_cpus::get().max(1),
```

Add a builder method:

```rust
    /// Set the number of worker tasks. `n.max(1)` is enforced.
    /// Plan 28.
    pub fn with_workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }
```

- [ ] **Step 2: Add same to `Coordinator`**

In `crates/ohara-core/src/indexer/coordinator/mod.rs`, add a `workers:
usize` field (default `num_cpus::get().max(1)` in `new`), a
`with_workers(n)` builder, and pass `self.workers` from
`Indexer::run` to `Coordinator::with_workers(...)`.

- [ ] **Step 3: Compile + smoke**

```
cargo build -p ohara-core -p ohara-cli
cargo test -p ohara-core
```

No behavior change yet — `run_timed_with_extractor` still uses the
old serial loop.

- [ ] **Step 4: Commit**

```bash
git add crates/ohara-core/src/indexer.rs crates/ohara-core/src/indexer/coordinator/mod.rs
git commit -m "feat(core): Indexer::with_workers + Coordinator plumbing (no behavior yet)"
```

---

### Task C.2 — Replace per-commit `for` loop with actor pipeline

**Files:**
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs`

This is the heart of the change. The existing
`run_timed_with_extractor` (around line 163) processes commits in a
serial `for` loop. We replace that loop with a walker task + N worker
tasks coordinated via `tokio::sync::mpsc`.

- [ ] **Step 1: Read the current loop**

Read `crates/ohara-core/src/indexer/coordinator/mod.rs` around lines
160-230. Note exactly:
- How `run_timed_with_extractor` calls `commit_walk`, then `for commit
  in &commits`, then `run_commit_timed` per commit.
- What gets accumulated in `result: CoordinatorResult`.
- Where `progress.commit_done(...)` ticks happen.

- [ ] **Step 2: Refactor `run_commit_timed` so it can be invoked from a worker**

`run_commit_timed` today takes `&self` plus `commit: &CommitMeta`. To
run inside a `tokio::spawn`'d task, the callable must be `'static`.
The simplest path: introduce a `run_commit_owned` async function that
takes ALL inputs by value (`Arc`s for shared state):

```rust
async fn run_commit_owned(
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    embed_mode: crate::EmbedMode,
    cache_storage: Option<Arc<dyn Storage>>,
    ignore_filter: Option<Arc<dyn crate::IgnoreFilter>>,
    repo: RepoId,
    commit: CommitMeta,
    ulid: ulid::Ulid,
    commit_source: Arc<dyn CommitSource>,
    symbol_source: Arc<dyn SymbolSource>,
    extractor: Arc<dyn AtomicSymbolExtractor>,
) -> Result<CommitWorkResult> {
    // Mirror the body of `run_commit_timed`, but:
    //   - Skip the existing-commit check (walker does it).
    //   - Pass `ulid.to_string()` into the `CommitRecord` built in
    //     the persist stage (already wired from A.4).
    //   - Apply ignore_filter the same way as today (plan-26).
    //   - Honor embed_mode (plan-27).
    //   - Return per-commit counters in `CommitWorkResult`.
    // The exact body is a refactor of the existing run_commit_timed,
    // not a rewrite. Preserve all existing stage calls.
    // ...
}

#[derive(Default)]
struct CommitWorkResult {
    new_hunks: usize,
    total_diff_bytes: u64,
    total_added_lines: u64,
    sha: String,
    succeeded: bool,
}
```

The reason for the `succeeded: bool` field: per-commit failure
isolation (spec D requires this). On error, the worker logs at
warn-level and returns `Ok(CommitWorkResult { succeeded: false, ... })`
rather than propagating the error up.

`CommitSource`, `SymbolSource`, and `AtomicSymbolExtractor` need to
be `Send + Sync + 'static` to live behind `Arc`. Most likely they
already are; verify by reading the trait definitions.

- [ ] **Step 3: Implement the actor topology**

Replace the body of `run_timed_with_extractor` (after the existing
`commit_walk` call but before the per-commit loop) with:

```rust
        let n_workers = self.workers.max(1);
        let (tx, rx) = tokio::sync::mpsc::channel::<(CommitMeta, ulid::Ulid)>(n_workers);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        // Walker task: emits (commit, ulid) for commits that don't
        // already exist.
        let storage_for_walker = self.storage.clone();
        let commits_owned: Vec<CommitMeta> = commits;
        let walker = tokio::spawn(async move {
            for commit in commits_owned {
                if storage_for_walker
                    .commit_exists(&commit.commit_sha)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                let ulid = crate::ulid_for_commit(commit.ts, &commit.commit_sha);
                if tx.send((commit, ulid)).await.is_err() {
                    break; // workers gone, channel closed
                }
            }
        });

        // Worker tasks.
        let mut workers = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let rx_for_worker = rx.clone();
            let storage = self.storage.clone();
            let embedder = self.embedder.clone();
            let embed_batch = self.embed_batch;
            let embed_mode = self.embed_mode;
            let cache_storage = self.cache_storage.clone();
            let ignore_filter = self.ignore_filter.clone();
            let commit_source_arc: Arc<dyn CommitSource> = /* see step 4 */;
            let symbol_source_arc: Arc<dyn SymbolSource> = /* see step 4 */;
            let extractor_arc: Arc<dyn AtomicSymbolExtractor> = /* see step 4 */;
            let repo_owned = repo.clone();
            workers.push(tokio::spawn(async move {
                let mut local = CommitWorkResult::default();
                loop {
                    let next = {
                        let mut guard = rx_for_worker.lock().await;
                        guard.recv().await
                    };
                    let Some((commit, ulid)) = next else { break };
                    match run_commit_owned(
                        storage.clone(),
                        embedder.clone(),
                        embed_batch,
                        embed_mode,
                        cache_storage.clone(),
                        ignore_filter.clone(),
                        repo_owned.clone(),
                        commit,
                        ulid,
                        commit_source_arc.clone(),
                        symbol_source_arc.clone(),
                        extractor_arc.clone(),
                    )
                    .await
                    {
                        Ok(r) => {
                            local.new_hunks += r.new_hunks;
                            local.total_diff_bytes += r.total_diff_bytes;
                            local.total_added_lines += r.total_added_lines;
                            if r.succeeded {
                                // (Worker fold of latest_sha is best-
                                // effort; the authoritative ordering
                                // is via ULID.)
                                local.sha = r.sha;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "plan-28 worker error; commit skipped");
                        }
                    }
                }
                local
            }));
        }

        walker.await.ok();
        for w in workers {
            if let Ok(local) = w.await {
                result.new_hunks += local.new_hunks;
                result.total_diff_bytes += local.total_diff_bytes;
                result.total_added_lines += local.total_added_lines;
                result.new_commits += if local.succeeded { 1 } else { 0 };
                if local.succeeded {
                    result.latest_sha = Some(local.sha);
                }
            }
        }
        Ok(result)
```

(Step 4 covers the "Arc the Source/Extractor" plumbing.)

- [ ] **Step 4: Make `CommitSource` / `SymbolSource` / `AtomicSymbolExtractor` Arc-friendly**

Confirm by reading `crates/ohara-core/src/indexer.rs` (around the
trait definitions) that each of these has `Send + Sync` bounds. If
they take `&self` only and are `Send + Sync`, they're fine to live
behind `Arc<dyn _>`.

The existing `Coordinator::run_timed_with_extractor` takes them as
`&dyn ...` borrowed references. To pass them into spawned tasks, we
need `Arc<dyn ...>`. Two paths:

(a) **Caller-side Arc.** The caller (in `Indexer::run`) builds
`Arc::new(commit_source)` and `Arc::new(symbol_source)` and the
coordinator's signature changes to accept `Arc`s.

(b) **Lifetime trick.** Keep `&dyn ...` and use `tokio::task::JoinSet`
with an `'static` requirement per spawned task — won't compile without
`Arc` because the borrow can't be `'static`.

Pick (a). Update `Coordinator::run_timed_with_extractor` to take
`Arc<dyn CommitSource>`, `Arc<dyn SymbolSource>`,
`Arc<dyn AtomicSymbolExtractor>`. Update `Indexer::run` to wrap each
in `Arc::new(...)` before calling. Update the Coordinator's tests
similarly.

- [ ] **Step 5: Run all tests**

```
cargo test -p ohara-core
```

The existing coordinator tests will need updating to construct
Arc-wrapped sources. Most test assertions are unchanged — the actor
pipeline must produce the same persisted state as the serial loop.

- [ ] **Step 6: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/indexer.rs \
        crates/ohara-core/src/indexer/coordinator/mod.rs \
        crates/ohara-core/src/indexer/coordinator/tests.rs
git commit -m "feat(core): actor-style commit pipeline (walker + N workers + mpsc channel)"
```

---

### Task C.3 — Per-commit failure isolation regression test

**File:** `crates/ohara-core/src/indexer/coordinator/tests.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn worker_error_on_one_commit_does_not_block_others() {
    // Plan 28 Task C.3: a CommitSource that fails for one specific
    // SHA. Expect 9 of 10 commits to persist; the failed one is
    // skipped and logged at warn level.
    use crate::EmbedMode;
    // Build storage, embedder, ten commits where commit 5 has a
    // "poisoned" SHA that the source treats as an error in
    // hunks_for_commit. The actor pipeline catches the error,
    // increments `result.new_commits` for the 9 successes, and the
    // 10th is left unindexed.
    // ...
}
```

(Flesh out the fixture using the existing `coordinator/tests.rs`
scaffolding — `SpyStorage`, `ZeroEmbedder`, etc. The poisoned source
returns `Err(...)` from `hunks_for_commit("poison-sha")`.)

- [ ] **Step 2: Run + commit**

```
cargo test -p ohara-core worker_error_on_one_commit_does_not_block_others
git add crates/ohara-core/src/indexer/coordinator/tests.rs
git commit -m "test(core): plan-28 — worker error on one commit doesn't block others"
```

---

## Phase D — CLI flag + e2e

### Task D.1 — `--workers <N>` clap arg on `ohara index`

**File:** `crates/ohara-cli/src/commands/index.rs`

- [ ] **Step 1: Add the field**

```rust
    /// Number of worker tasks for the parallel commit pipeline
    /// (plan-28). Defaults to the number of available CPUs.
    /// `--workers 1` reproduces the serial path.
    #[arg(long)]
    pub workers: Option<usize>,
```

- [ ] **Step 2: Plumb into the indexer**

In `index::run`, where `Indexer::new(...)` is constructed and chained
with `with_repo_root`, `with_embed_mode`, etc., chain:

```rust
        let indexer = /* existing builders */ ;
        let indexer = match args.workers {
            Some(n) => indexer.with_workers(n),
            None => indexer, // default = num_cpus::get(), set in `new`
        };
```

- [ ] **Step 3: Run + commit**

```
cargo run -p ohara-cli -- index --help    # confirms --workers visible
cargo build -p ohara-cli
cargo test -p ohara-cli
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-cli/src/commands/index.rs
# also stage any test fixture updates that need `workers: None`
git commit -m "feat(cli): ohara index --workers <N> for parallel pipeline"
```

---

### Task D.2 — E2E: `--workers 4` indexes a fixture in parallel

**File:** `crates/ohara-cli/tests/plan_28_parallel_indexer_e2e.rs` (NEW)

```rust
//! Plan-28 e2e: --workers 4 indexes a fixture with N commits and
//! all rows persist correctly. The MAX(ulid) commit_sha matches HEAD.

use std::path::Path;
use std::process::Command;

fn ohara_bin() -> String { env!("CARGO_BIN_EXE_ohara").to_string() }

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn parallel_indexer_with_4_workers_indexes_all_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    for i in 0..10 {
        std::fs::write(repo.join(format!("f{i}.txt")), format!("content {i}\n")).unwrap();
        Command::new("git").arg("-C").arg(repo).args(["add", "."]).output().unwrap();
        Command::new("git").arg("-C").arg(repo)
            .args(["-c", "user.email=a@a", "-c", "user.name=a",
                   "commit", "-m", &format!("commit {i}")])
            .output().unwrap();
    }
    let head = String::from_utf8(
        Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"])
            .output().unwrap().stdout).unwrap().trim().to_string();

    let idx = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--workers", "4"])
        .arg(repo)
        .output().unwrap();
    assert!(idx.status.success(),
        "ohara index failed: {}", String::from_utf8_lossy(&idx.stderr));

    let st = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .arg("status").arg(repo).output().unwrap();
    let stdout = String::from_utf8_lossy(&st.stdout);
    assert!(
        stdout.contains(&format!("last_indexed_commit: {head}")),
        "MAX(ulid) didn't match HEAD; status:\n{stdout}"
    );
}
```

- [ ] **Step 1: Write + run**

```
cargo test -p ohara-cli --test plan_28_parallel_indexer_e2e -- --include-ignored
git add crates/ohara-cli/tests/plan_28_parallel_indexer_e2e.rs
git commit -m "test(cli): plan-28 e2e — --workers 4 indexes all commits"
```

---

### Task D.3 — Regression: `--workers 1` matches serial behavior

Append to the same test file:

```rust
#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn workers_one_produces_same_row_counts_as_serial() {
    // Plan 28 Task D.3: --workers 1 must produce the same persisted
    // state (commit count, hunk count) as the serial path it
    // replaces. We can't compare wall-time meaningfully in a unit
    // test, but row-count parity is the contract.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    Command::new("git").arg("-C").arg(repo).args(["add", "."]).output().unwrap();
    Command::new("git").arg("-C").arg(repo)
        .args(["-c", "user.email=a@a", "-c", "user.name=a",
               "commit", "-m", "init"])
        .output().unwrap();

    let idx = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--workers", "1"])
        .arg(repo)
        .output().unwrap();
    assert!(idx.status.success());

    let st = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .arg("status").arg(repo).output().unwrap();
    let stdout = String::from_utf8_lossy(&st.stdout);
    assert!(stdout.contains("commits_behind_head: 0"));
}
```

```
cargo test -p ohara-cli --test plan_28_parallel_indexer_e2e -- --include-ignored
git add crates/ohara-cli/tests/plan_28_parallel_indexer_e2e.rs
git commit -m "test(cli): plan-28 regression — --workers 1 indexes correctly"
```

---

## Phase E — Perf harness + docs

### Task E.1 — `tests/perf/parallel_indexer_sweep.rs`

```rust
//! Plan-28 perf harness: --workers={1,2,4,8} sweep on a fixture.
//! Operator-run; not in CI.

use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "operator-run perf harness; opt in with --include-ignored"]
fn parallel_indexer_sweep() {
    let repo: PathBuf = std::env::var("OHARA_PERF_REPO")
        .expect("set OHARA_PERF_REPO to a real repo path").into();
    for n in &[1usize, 2, 4, 8] {
        let ohara_home = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let _ = Command::new(ohara_bin())
            .env("OHARA_HOME", ohara_home.path())
            .args(["index", "--rebuild", "--yes", "--embed-provider", "cpu",
                   "--workers", &n.to_string()])
            .arg(&repo).status().unwrap();
        eprintln!("workers={n} elapsed={:?}", start.elapsed());
    }
}
fn ohara_bin() -> String {
    let target = std::env::var("CARGO_BIN_EXE_ohara")
        .expect("test binary path");
    target
}
```

(Match the existing `tests/perf/` test-style harness pattern; if it
uses `[[bin]]` instead of `[[test]]`, mirror that.)

```bash
git add tests/perf/parallel_indexer_sweep.rs tests/perf/Cargo.toml
git commit -m "perf(plan-28): operator harness — --workers sweep"
```

---

### Task E.2 — Docs section in `indexing.md`

Append to `docs-book/src/architecture/indexing.md`:

```markdown
## Parallel commit pipeline (`--workers`)

`ohara index` runs a multi-worker pipeline by default:

- A walker task enumerates HEAD-reachable commits.
- N worker tasks each pull a commit and run the full pipeline
  (hunk_chunk → attribute → embed → persist) end-to-end.
- A bounded mpsc channel (capacity = N) provides backpressure.

Each commit gets a deterministic ULID derived from `(commit_time,
commit_sha)`. Persistence is order-free; the read path recovers
chronological order via `ORDER BY ulid`. Resume falls back to plan-9's
`commit_exists` skip — already-indexed commits are dropped from the
walker output.

Default: `--workers $(num_cpus)`. Use `--workers 1` for the serial
path (matches today's behavior; useful for debugging). The big speedup
shows up when the chunk-embed cache (`--embed-cache=semantic|diff`,
plan-27) is warm — most chunks then become cache lookups and parse
becomes the dominant per-commit cost, fanning out across workers.
```

```bash
git add docs-book/src/architecture/indexing.md
git commit -m "docs(plan-28): document --workers flag"
```

---

## Pre-completion checklist

Per `CONTRIBUTING.md` §13:

- [ ] `cargo fmt --all` clean.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --workspace` green (model-loading e2e tests gated under
      `--ignored` per plan-26 / plan-27 convention).
- [ ] No file > 500 lines (especially
      `crates/ohara-core/src/indexer/coordinator/mod.rs` after the
      actor refactor — split if needed).
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code (the
      `expect("invariant: commit_sha is 40-hex")` form is allowed).
- [ ] No `println!` outside `ohara-cli` user-facing output.
- [ ] Workspace-only deps: `ulid` and `num_cpus` in workspace; new code
      uses `dep.workspace = true`.
- [ ] `--workers 1` produces same persisted state as today's serial
      path (regression test in D.3).
- [ ] Per-commit failure isolation works (regression test in C.3).
- [ ] `cargo build --release` clean.

## Out of scope

- ULID backfill for pre-V6 rows. Documented as a future
  `ohara reindex --backfill-ulid` follow-up; not needed because
  retrieval doesn't use ULID.
- `ohara status workers:` line.
- Per-stage tuning (`--parse-workers` / `--embed-workers`).
- Embed batch pooling across workers.
