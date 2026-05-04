# ohara plan-21 — Explain hydrator + ContentHash + BlameCache wiring

> **Status:** draft

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per
> repo conventions: commit after each red test and again after each
> green implementation.

**Goal:** Refactor `ohara_core::explain::explain_change` so blame
computation and result hydration become independently testable, introduce
a `ContentHash` newtype keyed by git blob OID, and wire the existing
`ohara_engine::cache::BlameCache` (built but inert in plan-16 E.1) so
daemon-warm `explain_change` calls skip `Blamer::blame_range` when the
file's HEAD content has not changed.

**Architecture:**

- New: `crates/ohara-core/src/explain/hydrator.rs` — exposes
  `pub async fn hydrate_blame_results(storage, blame_ranges, query) ->
  Result<HydratedBlame>`. Takes a batched `Vec<BlameRange>`, returns
  `HydratedBlame { hits, coverage, limitation, enrichment_limitation }`.
  Pure function over storage and the input ranges. No git dependency.

- New: `crates/ohara-core/src/types::ContentHash(String)` — hex of git
  blob OID. Opaque newtype; `Hash + Eq + Clone` so it works as a cache
  key. Exposes `from_blob_oid(git2::Oid) -> Self` and
  `from_hex(&str) -> Self`.

- New Storage trait method: `Storage::get_commits_by_sha(&[CommitSha])
  -> Result<HashMap<CommitSha, CommitMeta>>`. Trait ships with a
  default loop-over-`get_commit` implementation. `SqliteStorage` gets a
  real batched `IN (?, …)` impl — single round-trip. Used by the
  hydrator to avoid N storage round-trips per blame result.

- `crates/ohara-core/src/explain/orchestrator.rs` — slim replacement
  for the current monolithic `explain_change`. Calls `Blamer::blame_range`
  then delegates to `hydrate_blame_results` then assembles
  `ExplainResponse`. File stays under 500 lines.

- `crates/ohara-engine/src/engine.rs::explain_change` — adds the
  BlameCache check: computes `ContentHash` from
  `git2::Tree::get_path(file)?.id()` opened from `handle.repo_path`,
  builds key `(repo_id, file, content_hash)`, consults `self.blame_cache`.
  On hit, returns cached `Arc<Vec<BlameRange>>` and runs only the
  hydrator. On miss, calls Blamer, stores result in the cache, then
  hydrator.

**Tech stack:** Rust 2021, existing git2 / thiserror / tokio. No new
third-party deps.

**Spec:** none — internal refactor from the /improve-codebase-architecture
audit (candidate #3, "Explain blame seam"). The `BlameCache` structure
was built in plan-16 E.1 but its wiring was explicitly deferred to this
plan.

**Scope check:** plan-21 is explain-side only. Independent from:

- plan-19 (Indexer 5-stage pipeline)
- plan-20 (Retriever lanes + ScoreRefiner)

---

## Phase A — `ContentHash` newtype

### Task A.1 — Add `ContentHash` to `ohara-core::types`

**Files:**

- Modify: `crates/ohara-core/src/types.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/ohara-core/src/types.rs` (below the existing `symbol_tests` module):

```rust
#[cfg(test)]
mod content_hash_tests {
    use super::*;

    #[test]
    fn from_hex_round_trips_as_str() {
        // Plan 21 Task A.1: ContentHash constructed from a known hex
        // string must echo it back via as_str() unchanged.
        let h = ContentHash::from_hex("deadbeef1234");
        assert_eq!(h.as_str(), "deadbeef1234");
    }

    #[test]
    fn content_hash_is_eq_and_hash() {
        // Plan 21 Task A.1: ContentHash must be usable as a HashMap key
        // (requires Hash + Eq). Two values built from the same hex string
        // must be equal; two different hex strings must differ.
        use std::collections::HashMap;
        let a = ContentHash::from_hex("aaa");
        let b = ContentHash::from_hex("aaa");
        let c = ContentHash::from_hex("bbb");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut m: HashMap<ContentHash, u8> = HashMap::new();
        m.insert(a.clone(), 1);
        assert_eq!(*m.get(&b).expect("must find by equal key"), 1);
    }

    #[test]
    fn from_blob_oid_produces_40_char_hex() {
        // Plan 21 Task A.1: from_blob_oid wraps git2::Oid::from_str,
        // which produces 40-character hex. Verify the length contract.
        let oid = git2::Oid::from_str("a" .repeat(40).as_str()).expect("valid oid");
        let h = ContentHash::from_blob_oid(oid);
        assert_eq!(h.as_str().len(), 40);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ohara-core content_hash_tests`
Expected: FAIL — `ContentHash` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/types.rs
git commit -m "test(core): plan-21 ContentHash newtype contract (failing)"
```

- [ ] **Step 4: Implement `ContentHash`**

Append to `crates/ohara-core/src/types.rs` (above the existing `#[cfg(test)]` blocks):

```rust
/// Plan 21: opaque newtype wrapping the hex representation of a git blob
/// OID. Used as the file-content key in `BlameCache`.
///
/// Two `ContentHash` values are equal iff they were produced from the same
/// blob OID — which means the file's byte content is identical. The cache
/// provides natural invalidation: a file whose content changes gets a new
/// blob OID, producing a cache miss and a fresh `Blamer::blame_range` call.
///
/// `from_blob_oid` is the only constructor for production callers (where a
/// real `git2::Oid` is available). `from_hex` exists for test and non-git
/// callers that hold a pre-computed hex string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    /// Construct from a git blob OID. The resulting hex string is 40 ASCII
    /// characters (SHA-1) — the length guaranteed by `git2::Oid`.
    pub fn from_blob_oid(oid: git2::Oid) -> Self {
        Self(oid.to_string())
    }

    /// Construct from an already-computed hex string. No validation is
    /// performed — callers are responsible for passing a valid hex OID.
    pub fn from_hex(hex: &str) -> Self {
        Self(hex.to_string())
    }

    /// Borrow the underlying hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
```

Also add `git2` to `ohara-core`'s dependency list if not already present.
Check `crates/ohara-core/Cargo.toml`; if `git2` is absent add:

```toml
# crates/ohara-core/Cargo.toml
[dependencies]
git2 = { workspace = true }
```

Verify `git2` is in the workspace root `Cargo.toml`
`[workspace.dependencies]`; it is already there (used by `ohara-git`).

- [ ] **Step 5: Re-export from `ohara-core::types` public surface**

In `crates/ohara-core/src/lib.rs`, find the `pub use types::{ … }` line
and add `ContentHash`:

```rust
pub use types::{
    AttributionKind, ChangeKind, CommitMeta, ContentHash, Hunk, HunkSymbol,
    Provenance, RepoId, Symbol, SymbolKind,
};
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p ohara-core content_hash_tests`
Expected: PASS (all three tests).

- [ ] **Step 7: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS — all existing tests unaffected.

- [ ] **Step 8: Commit**

```bash
git add crates/ohara-core/src/types.rs crates/ohara-core/src/lib.rs \
        crates/ohara-core/Cargo.toml
git commit -m "feat(core): ContentHash newtype from git blob OID (plan-21)"
```

---

## Phase B — `Storage::get_commits_by_sha` batched lookup

### Task B.1 — Trait method with default impl + SqliteStorage batched impl

**Files:**

- Modify: `crates/ohara-core/src/storage.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-storage/src/tables/explain.rs` (or wherever
  `get_commit` SQL lives — verify before editing)

- [ ] **Step 1: Write the failing test in `ohara-storage`**

Append to `crates/ohara-storage/src/storage_impl.rs` or a dedicated
`tests/explain.rs` integration test — choose whichever file already
holds integration tests for `get_commit`. Append:

```rust
#[tokio::test]
async fn get_commits_by_sha_returns_all_three_rows() {
    // Plan 21 Task B.1: SqliteStorage's batched implementation must
    // return all three seeded commits in one round-trip. Uses an
    // in-memory database (":memory:" path).
    use ohara_core::types::{CommitMeta, RepoId};
    use ohara_core::Storage;

    let tmp = tempfile::tempdir().unwrap();
    let db = crate::SqliteStorage::open(tmp.path().join("test.db"))
        .await
        .unwrap();
    let rid = RepoId::from_parts("deadbeef", "/x");
    db.open_repo(&rid, "/x", "deadbeef").await.unwrap();

    let shas = ["aaa000", "bbb000", "ccc000"];
    for (i, sha) in shas.iter().enumerate() {
        db.put_commit(
            &rid,
            &ohara_core::storage::CommitRecord {
                meta: CommitMeta {
                    commit_sha: sha.to_string(),
                    parent_sha: None,
                    is_merge: false,
                    author: Some("alice".into()),
                    ts: i as i64 * 1_000,
                    message: format!("msg {sha}"),
                },
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
    }

    let result = db
        .get_commits_by_sha(&rid, &shas.map(String::from))
        .await
        .unwrap();
    assert_eq!(result.len(), 3, "all three rows must be returned");
    for sha in &shas {
        assert!(result.contains_key(*sha), "missing sha {sha}");
    }
    // Unknown SHA is absent, not an error.
    let unknown = db
        .get_commits_by_sha(&rid, &["notexist".to_string()])
        .await
        .unwrap();
    assert!(unknown.is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-storage get_commits_by_sha_returns_all_three_rows`
Expected: FAIL — method does not exist yet.

- [ ] **Step 3: Commit the red test**

```bash
git add crates/ohara-storage/
git commit -m "test(storage): plan-21 get_commits_by_sha batched contract (failing)"
```

- [ ] **Step 4: Add the trait method with a default loop impl**

In `crates/ohara-core/src/storage.rs`, append below `get_commit` (around
line 269):

```rust
    /// Plan 21: fetch multiple commits by SHA in one call. Returns a
    /// `HashMap` mapping each found SHA to its `CommitMeta`. SHAs that
    /// are not indexed produce no entry — callers treat absence as
    /// "unindexed" (same semantics as `get_commit` returning `Ok(None)`).
    ///
    /// The default implementation calls `get_commit` once per SHA
    /// (preserves backward compatibility for `MockStorage` and other
    /// test fakes that don't override this method). Production callers
    /// should use `SqliteStorage` whose override issues a single SQL
    /// statement with `IN (?, …)`.
    async fn get_commits_by_sha(
        &self,
        repo_id: &RepoId,
        shas: &[String],
    ) -> Result<std::collections::HashMap<String, CommitMeta>> {
        let mut out = std::collections::HashMap::with_capacity(shas.len());
        for sha in shas {
            if let Some(cm) = self.get_commit(repo_id, sha).await? {
                out.insert(sha.clone(), cm);
            }
        }
        Ok(out)
    }
```

Note: `async fn` in trait default bodies requires the `async_trait`
macro to be in scope. If the trait already uses `#[async_trait]`,
this compiles as-is. Verify the trait annotation at the top of the
`Storage` declaration; if missing, add:

```rust
#[async_trait]
pub trait Storage: Send + Sync {
```

The default body uses `self.get_commit` which is also `async` and
inside the same `async_trait` expansion — this pattern already works
in the existing trait (e.g. `metrics_snapshot` uses non-async default).
Since the default is `async`, it must go through `async_trait` expansion;
the macro handles this transparently.

- [ ] **Step 5: Add `SqliteStorage` batched override**

In `crates/ohara-storage/src/storage_impl.rs` (or wherever
`get_commit` is implemented for `SqliteStorage`), add:

```rust
    async fn get_commits_by_sha(
        &self,
        repo_id: &RepoId,
        shas: &[String],
    ) -> ohara_core::Result<std::collections::HashMap<String, ohara_core::types::CommitMeta>> {
        if shas.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // Build `IN (?, ?, …)` with one placeholder per SHA so all rows
        // are fetched in a single SQLite round-trip. All SQL stays in
        // ohara-storage per CONTRIBUTING.md.
        let placeholders = shas
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT cr.sha, cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message \
             FROM commit_record cr \
             WHERE cr.repo_id = (SELECT id FROM repo WHERE repo_id = ?) \
               AND cr.sha IN ({placeholders})"
        );
        // Bind repo_id first, then each sha.
        // Use the existing `self.pool` / `self.conn` pattern from the
        // surrounding file — mirror exactly how `get_commit` binds its
        // single-sha query, extending to N params.
        //
        // Implementation note: `sqlx` supports binding Vec<&str> via
        // a query builder; `rusqlite` requires manual binding. Mirror
        // the pattern already in use in this file for `get_commit`.
        // The concrete binding code below uses the `rusqlite` Tokio
        // shim already in `SqliteStorage`. Adjust to match whichever
        // async SQLite adapter is in use (check the imports at the top
        // of storage_impl.rs).
        let repo_id_str = repo_id.as_str().to_string();
        let shas_owned: Vec<String> = shas.to_vec();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                Vec::with_capacity(1 + shas_owned.len());
            params.push(Box::new(repo_id_str.clone()));
            for sha in &shas_owned {
                params.push(Box::new(sha.clone()));
            }
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok(ohara_core::types::CommitMeta {
                    commit_sha: row.get(0)?,
                    parent_sha: row.get(1)?,
                    is_merge: row.get::<_, i64>(2)? != 0,
                    author: row.get(3)?,
                    ts: row.get(4)?,
                    message: row.get(5)?,
                })
            })?;
            let mut out = std::collections::HashMap::new();
            for r in rows {
                let cm = r?;
                out.insert(cm.commit_sha.clone(), cm);
            }
            Ok(out)
        })
        .await
    }
```

Implementation note: `SqliteStorage` uses a `with_conn` helper pattern
(visible in the surrounding file) that runs a closure on a blocking
thread. Mirror that pattern exactly. If the adapter is `sqlx` instead,
rewrite using `sqlx::query_as` with a dynamically-built IN clause.
Check the top of `storage_impl.rs` imports to confirm.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p ohara-storage get_commits_by_sha_returns_all_three_rows`
Expected: PASS.

- [ ] **Step 7: Run the full storage suite**

Run: `cargo test -p ohara-storage`
Expected: PASS — existing explain tests unaffected (they still go through
`get_commit`, which is unchanged).

- [ ] **Step 8: Commit**

```bash
git add crates/ohara-core/src/storage.rs crates/ohara-storage/src/storage_impl.rs
git commit -m "feat(storage): batched get_commits_by_sha; default loop + SqliteStorage IN-clause (plan-21)"
```

---

## Phase C — `hydrate_blame_results` extraction

### Task C.1 — Extract inline helpers to `explain/hydrator.rs`

Move `build_limitation` and `collect_related_commits` out of `explain.rs`
into a new module. Today's unit tests must still pass after the move.

**Files:**

- Create: `crates/ohara-core/src/explain/hydrator.rs`
- Modify: `crates/ohara-core/src/explain.rs` (becomes
  `src/explain/mod.rs` if the directory is created, OR remains flat
  with a `mod hydrator;` declaration — choose whichever keeps line
  counts under 500)
- Modify: `crates/ohara-core/src/lib.rs` (if module path changes)

Before editing, count lines in `explain.rs`:

```bash
wc -l crates/ohara-core/src/explain.rs
```

`explain.rs` is currently ~1012 lines — above the 500-line limit.
Convert to a module directory:

```
crates/ohara-core/src/explain/
    mod.rs          ← re-exports, existing BlameSource / ExplainQuery / ExplainHit / ExplainMeta / RelatedCommit / explain_change
    hydrator.rs     ← build_limitation, collect_related_commits (moved here)
    orchestrator.rs ← explain_change thin wrapper (Phase D)
```

- [ ] **Step 1: Verify the current test suite passes before touching anything**

Run: `cargo test -p ohara-core`
Expected: PASS. (Baseline.)

- [ ] **Step 2: Create the directory structure**

```bash
mkdir -p crates/ohara-core/src/explain
mv crates/ohara-core/src/explain.rs crates/ohara-core/src/explain/mod.rs
```

Verify the crate still compiles:

```bash
cargo build -p ohara-core
```

If `lib.rs` references `mod explain;` it will keep working because the
compiler resolves both `explain.rs` and `explain/mod.rs`. No `lib.rs`
change required unless the path moved.

- [ ] **Step 3: Create `hydrator.rs` with the extracted helpers**

Create `crates/ohara-core/src/explain/hydrator.rs`:

```rust
//! Plan 21: hydration helpers extracted from `explain/mod.rs`.
//!
//! These were private free functions in the monolithic `explain.rs`.
//! Extracting them lets the hydrator be unit-tested with a `MockStorage`
//! without running a real `Blamer::blame_range`.

use crate::explain::{ExplainHit, RelatedCommit};
use crate::storage::Storage;
use crate::types::{Provenance, RepoId};
use crate::Result;

/// Build the `_meta.limitation` string from blame statistics.
///
/// Called from `hydrate_blame_results` and (transitionally) still
/// called from `explain_change` in `mod.rs` until Phase D slim-down.
pub(crate) fn build_limitation(
    total: u32,
    skipped: &[String],
    clamped_start: u32,
    clamped_end: u32,
) -> Option<String> {
    if total == 0 {
        return Some(
            "blame returned no attributable lines \
             (file missing in HEAD or empty range)"
                .into(),
        );
    }
    if !skipped.is_empty() {
        let n = skipped.len();
        let preview: Vec<&str> = skipped.iter().take(3).map(String::as_str).collect();
        let suffix = if n > preview.len() {
            format!(" (+{} more)", n - preview.len())
        } else {
            String::new()
        };
        return Some(format!(
            "{n} commit(s) older than the local index watermark were skipped: \
             [{}]{}; range covered: {clamped_start}..={clamped_end}",
            preview.join(", "),
            suffix,
        ));
    }
    None
}

/// Plan 12 Task 3.2 logic, now living in `hydrator.rs`.
///
/// Collects contextual neighbours per blame anchor. Per-anchor limits
/// (2 before / 2 after) and overall dedup-by-sha keep the response
/// payload bounded. Returns `(related, enrichment_limitation)`.
pub(crate) async fn collect_related_commits(
    storage: &dyn Storage,
    repo_id: &RepoId,
    file: &str,
    hits: &[ExplainHit],
) -> Result<(Vec<RelatedCommit>, Option<String>)> {
    use chrono::{DateTime, Utc};
    use std::collections::BTreeSet;
    const NEIGHBOURS_BEFORE: u8 = 2;
    const NEIGHBOURS_AFTER: u8 = 2;

    let anchor_shas: BTreeSet<&str> = hits.iter().map(|h| h.commit_sha.as_str()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<RelatedCommit> = Vec::new();

    for hit in hits {
        let neighbours = storage
            .get_neighboring_file_commits(
                repo_id,
                file,
                &hit.commit_sha,
                NEIGHBOURS_BEFORE,
                NEIGHBOURS_AFTER,
            )
            .await?;
        for (touched, cm) in neighbours {
            if anchor_shas.contains(cm.commit_sha.as_str()) {
                continue;
            }
            if !seen.insert(cm.commit_sha.clone()) {
                continue;
            }
            let date = DateTime::<Utc>::from_timestamp(cm.ts, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default();
            out.push(RelatedCommit {
                commit_sha: cm.commit_sha,
                commit_message: cm.message,
                commit_author: cm.author,
                commit_date: date,
                touched_hunks: touched,
                provenance: Provenance::Inferred,
            });
        }
    }
    Ok((out, None))
}
```

- [ ] **Step 4: Update `explain/mod.rs` to delegate to the new module**

In `crates/ohara-core/src/explain/mod.rs`, add at the top (below
the existing `use` statements):

```rust
pub(crate) mod hydrator;
```

Replace the existing `build_limitation` and `collect_related_commits`
function bodies with delegation to `hydrator::`:

```rust
fn build_limitation(
    total: u32,
    skipped: &[String],
    clamped_start: u32,
    clamped_end: u32,
) -> Option<String> {
    hydrator::build_limitation(total, skipped, clamped_start, clamped_end)
}

async fn collect_related_commits(
    storage: &dyn Storage,
    repo_id: &RepoId,
    file: &str,
    hits: &[ExplainHit],
) -> Result<(Vec<RelatedCommit>, Option<String>)> {
    hydrator::collect_related_commits(storage, repo_id, file, hits).await
}
```

(The wrappers keep the call-sites inside `explain_change` unchanged
for this task; Phase D removes the wrappers.)

- [ ] **Step 5: Run the full suite to confirm no regression**

Run: `cargo test -p ohara-core`
Expected: PASS — all existing tests pass unchanged. Existing golden
tests in `ohara-mcp` also pass:

Run: `cargo test -p ohara-mcp`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/explain/ crates/ohara-core/src/lib.rs
git commit -m "refactor(core): extract explain helpers to hydrator module (plan-21 C.1)"
```

---

### Task C.2 — `hydrate_blame_results` public function + unit test

**Files:**

- Modify: `crates/ohara-core/src/explain/hydrator.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-core/src/explain/hydrator.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::explain::{BlameRange, ExplainQuery};
    use crate::storage::{CommitRecord, HunkHit, HunkRecord};
    use crate::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol};
    use crate::query::IndexStatus;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal storage fake: knows about two commits, no hunks (diff
    /// excerpt path skipped by include_diff=false).
    struct TwoCommitStorage {
        commits: HashMap<String, CommitMeta>,
    }

    impl TwoCommitStorage {
        fn with(pairs: &[(&str, i64, &str)]) -> Self {
            let mut commits = HashMap::new();
            for &(sha, ts, msg) in pairs {
                commits.insert(
                    sha.to_string(),
                    CommitMeta {
                        commit_sha: sha.into(),
                        parent_sha: None,
                        is_merge: false,
                        author: None,
                        ts,
                        message: msg.into(),
                    },
                );
            }
            Self { commits }
        }
    }

    #[async_trait]
    impl crate::storage::Storage for TwoCommitStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> { unreachable!() }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> { unreachable!() }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> { unreachable!() }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { unreachable!() }
        async fn bm25_hunks_by_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { unreachable!() }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { unreachable!() }
        async fn bm25_hunks_by_symbol_name(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { unreachable!() }
        async fn bm25_hunks_by_historical_symbol(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { unreachable!() }
        async fn get_hunk_symbols(&self, _: &RepoId, _: crate::storage::HunkId) -> crate::Result<Vec<crate::types::HunkSymbol>> { unreachable!() }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, sha: &str) -> crate::Result<Option<CommitMeta>> {
            Ok(self.commits.get(sha).cloned())
        }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<Vec<Hunk>> {
            Ok(Vec::new()) // include_diff=false, so never called
        }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> crate::Result<Vec<(u32, CommitMeta)>> {
            Ok(Vec::new())
        }
        async fn get_index_metadata(&self, _: &RepoId) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
            Ok(crate::index_metadata::StoredIndexMetadata::default())
        }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> { Ok(()) }
    }

    #[tokio::test]
    async fn hydrate_blame_results_returns_two_hits_and_full_coverage() {
        // Plan 21 Task C.2: synthetic Vec<BlameRange> covering 2 SHAs
        // (both indexed) → hits.len() == 2, coverage == 1.0, no limitation.
        let storage = TwoCommitStorage::with(&[
            ("sha1", 1_000, "first commit"),
            ("sha2", 2_000, "second commit"),
        ]);
        let ranges = vec![
            BlameRange { commit_sha: "sha1".into(), lines: vec![1, 2] },
            BlameRange { commit_sha: "sha2".into(), lines: vec![3, 4] },
        ];
        let query = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 4,
            k: 5,
            include_diff: false,
            include_related: false,
        };
        let repo_id = RepoId::from_parts("aaa", "/r");
        let h = hydrate_blame_results(&storage, ranges, &query, &repo_id)
            .await
            .unwrap();

        assert_eq!(h.hits.len(), 2);
        assert!(
            (h.coverage - 1.0_f32).abs() < 1e-6,
            "full coverage when all SHAs are indexed"
        );
        assert!(h.limitation.is_none(), "no limitation when nothing is skipped");
        assert!(h.enrichment_limitation.is_none());
    }

    #[tokio::test]
    async fn hydrate_blame_results_partial_coverage_sets_limitation() {
        // One SHA indexed, one not — coverage 0.5, limitation present.
        let storage = TwoCommitStorage::with(&[("known", 1_000, "known")]);
        let ranges = vec![
            BlameRange { commit_sha: "known".into(),   lines: vec![1, 2] },
            BlameRange { commit_sha: "unknown".into(), lines: vec![3, 4] },
        ];
        let query = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 4,
            k: 5,
            include_diff: false,
            include_related: false,
        };
        let repo_id = RepoId::from_parts("aaa", "/r");
        let h = hydrate_blame_results(&storage, ranges, &query, &repo_id)
            .await
            .unwrap();

        assert_eq!(h.hits.len(), 1);
        assert!(
            (h.coverage - 0.5_f32).abs() < 1e-6,
            "half the lines are indexed"
        );
        assert!(h.limitation.is_some());
    }
}
```

- [ ] **Step 2: Run the failing test**

Run: `cargo test -p ohara-core hydrate_blame_results`
Expected: FAIL — `hydrate_blame_results` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/explain/hydrator.rs
git commit -m "test(core): plan-21 hydrate_blame_results contract (failing)"
```

- [ ] **Step 4: Implement `HydratedBlame` and `hydrate_blame_results`**

Prepend to the existing content in `hydrator.rs` (above `build_limitation`):

```rust
use crate::diff_text::{truncate_diff, DIFF_EXCERPT_MAX_LINES};
use crate::types::Provenance;
use chrono::{DateTime, Utc};

/// Output of `hydrate_blame_results`. Mirrors the shape that
/// `explain_change` assembles from inline variables today; extracting
/// it into a named struct lets `explain/orchestrator.rs` compose the
/// final `(Vec<ExplainHit>, ExplainMeta)` without re-reading storage.
pub struct HydratedBlame {
    /// Enriched hits, ordered as they came from the blame ranges (sort
    /// to newest-first happens in the orchestrator after hydration so
    /// the orchestrator controls the `k` cap logic).
    pub hits: Vec<super::ExplainHit>,
    /// Fraction of blame-attributed lines that resolved to an indexed
    /// commit. 1.0 means full attribution; <1.0 means some SHAs were
    /// absent from the index.
    pub coverage: f32,
    /// Set when any lines were missed (file not found, unindexed SHAs).
    pub limitation: Option<String>,
    /// Set when the related-commit enrichment was constrained.
    pub enrichment_limitation: Option<String>,
    /// Contextual neighbours from `collect_related_commits`. Empty when
    /// `query.include_related` is false.
    pub related_commits: Vec<super::RelatedCommit>,
    /// Clamped line range derived from the blame output.
    pub clamped_range: (u32, u32),
}

/// Hydrate a pre-computed `Vec<BlameRange>` into `HydratedBlame`.
///
/// Deliberately does NOT call `BlameSource::blame_range` — callers
/// (the orchestrator or the engine's cache path) supply ranges that
/// were already computed. This is the seam that makes the BlameCache
/// wiring in Phase E possible: cached ranges bypass the blamer and go
/// straight here.
///
/// Uses `Storage::get_commits_by_sha` (Task B.1) to resolve all SHAs
/// in a single batched storage call.
pub async fn hydrate_blame_results(
    storage: &dyn Storage,
    blame_ranges: Vec<super::BlameRange>,
    query: &super::ExplainQuery,
    repo_id: &RepoId,
) -> Result<HydratedBlame> {
    // Derive the clamped range and line attribution totals from the blame
    // output — mirrors the existing logic in `explain_change`.
    let (clamped_start, clamped_end, lines_attributed_total) = if blame_ranges.is_empty() {
        (query.line_start, query.line_end, 0u32)
    } else {
        let mut min_line = u32::MAX;
        let mut max_line = 0u32;
        let mut total = 0u32;
        for r in &blame_ranges {
            for &l in &r.lines {
                if l < min_line { min_line = l; }
                if l > max_line { max_line = l; }
                total += 1;
            }
        }
        if min_line == u32::MAX {
            (query.line_start, query.line_end, 0)
        } else {
            (min_line, max_line, total)
        }
    };

    // Batch-resolve all unique SHAs in one storage round-trip.
    let shas: Vec<String> = blame_ranges.iter().map(|r| r.commit_sha.clone()).collect();
    let commit_map = storage.get_commits_by_sha(repo_id, &shas).await?;

    let mut hits: Vec<super::ExplainHit> = Vec::with_capacity(blame_ranges.len());
    let mut skipped_shas: Vec<String> = Vec::new();
    let mut lines_attributed_indexed: u32 = 0;

    for r in blame_ranges {
        match commit_map.get(&r.commit_sha) {
            Some(cm) => {
                lines_attributed_indexed += r.lines.len() as u32;
                let (excerpt, truncated) = if query.include_diff {
                    let hunks = storage
                        .get_hunks_for_file_in_commit(repo_id, &cm.commit_sha, &query.file)
                        .await?;
                    let joined: String = hunks
                        .iter()
                        .map(|h| h.diff_text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    truncate_diff(&joined, DIFF_EXCERPT_MAX_LINES)
                } else {
                    (String::new(), false)
                };
                let date = DateTime::<Utc>::from_timestamp(cm.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                hits.push(super::ExplainHit {
                    commit_sha: cm.commit_sha.clone(),
                    commit_message: cm.message.clone(),
                    commit_author: cm.author.clone(),
                    commit_date: date,
                    blame_lines: r.lines,
                    file_path: query.file.clone(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    provenance: Provenance::Exact,
                });
            }
            None => {
                tracing::debug!(
                    sha = %r.commit_sha,
                    "hydrate_blame_results: skipping unindexed commit"
                );
                skipped_shas.push(r.commit_sha);
            }
        }
    }

    let coverage = if lines_attributed_total == 0 {
        0.0
    } else {
        lines_attributed_indexed as f32 / lines_attributed_total as f32
    };
    let limitation = build_limitation(
        lines_attributed_total,
        &skipped_shas,
        clamped_start,
        clamped_end,
    );

    let (related_commits, enrichment_limitation) = if !query.include_related {
        (Vec::new(), None)
    } else if hits.is_empty() {
        (
            Vec::new(),
            Some("no indexed blame anchors — no contextual neighbours available".into()),
        )
    } else {
        collect_related_commits(storage, repo_id, &query.file, &hits).await?
    };

    Ok(HydratedBlame {
        hits,
        coverage,
        limitation,
        enrichment_limitation,
        related_commits,
        clamped_range: (clamped_start, clamped_end),
    })
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core hydrate_blame_results`
Expected: PASS (both tests).

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/explain/hydrator.rs
git commit -m "feat(core): hydrate_blame_results — batched storage + HydratedBlame (plan-21 C.2)"
```

---

## Phase D — Orchestrator slim-down

### Task D.1 — Replace the inline body of `explain_change` with the hydrator

**Files:**

- Modify: `crates/ohara-core/src/explain/mod.rs` (or the new location
  after the directory split in Task C.1)

- [ ] **Step 1: Verify the existing orchestrator tests still pass**

Run: `cargo test -p ohara-core explain`
Expected: PASS — this is the green baseline before slimming.

- [ ] **Step 2: Replace `explain_change` body**

In `crates/ohara-core/src/explain/mod.rs`, replace the current
`explain_change` implementation (roughly lines 157-313 of the original
file, or its equivalent in `mod.rs` after the split) with:

```rust
/// Run an `explain_change` query end-to-end.
///
/// 1. Ask the `BlameSource` for line ownership over the queried range.
/// 2. Delegate all storage hydration to `hydrator::hydrate_blame_results`.
/// 3. Sort hits newest-first by `commit_date`, cap to `query.k`.
/// 4. Assemble and return `(Vec<ExplainHit>, ExplainMeta)`.
///
/// The BlameCache wiring (skipping step 1 on a cache hit) lives in
/// `ohara_engine::engine::explain_change` — the core orchestrator
/// always runs the blamer; it is the engine's responsibility to short-
/// circuit when cached ranges are available.
pub async fn explain_change(
    storage: &dyn Storage,
    blamer: &dyn BlameSource,
    repo_id: &RepoId,
    query: &ExplainQuery,
) -> Result<(Vec<ExplainHit>, ExplainMeta)> {
    use crate::perf_trace::timed_phase;

    let raw_blame = timed_phase(
        "blame",
        blamer.blame_range(&query.file, query.line_start, query.line_end),
    )
    .await?;

    let hydrated = timed_phase(
        "hydrate_explain",
        hydrator::hydrate_blame_results(storage, raw_blame, query, repo_id),
    )
    .await?;

    let mut hits = hydrated.hits;
    hits.sort_by(|a, b| match b.commit_date.cmp(&a.commit_date) {
        std::cmp::Ordering::Equal => a.commit_sha.cmp(&b.commit_sha),
        other => other,
    });
    let k = query.k.clamp(1, K_MAX) as usize;
    hits.truncate(k);

    let meta = ExplainMeta {
        lines_queried: hydrated.clamped_range,
        commits_unique: hits.len(),
        blame_coverage: hydrated.coverage,
        limitation: hydrated.limitation,
        related_commits: hydrated.related_commits,
        enrichment_limitation: hydrated.enrichment_limitation,
    };
    Ok((hits, meta))
}
```

Also remove the now-dead inline helpers `build_limitation` and
`collect_related_commits` from `mod.rs` (they live in `hydrator.rs`).
The delegating wrappers added in Task C.1 can be deleted.

- [ ] **Step 3: Run the orchestrator tests to confirm they pass unchanged**

Run: `cargo test -p ohara-core explain`
Expected: PASS — all eight existing orchestrator tests still green.
Run: `cargo test -p ohara-mcp`
Expected: PASS — `envelope_parity` golden tests unchanged.

- [ ] **Step 4: Check line count**

Run: `wc -l crates/ohara-core/src/explain/mod.rs`
Expected: under 500 lines. If it exceeds 500, move the test module
to a separate `explain/tests.rs` file (use `#[cfg(test)] mod tests`
in `mod.rs` pointing to `tests.rs` via `include!` is not idiomatic —
instead use `mod tests;` and move the test module to `tests.rs`).

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/explain/
git commit -m "refactor(core): slim explain_change; delegate to hydrator (plan-21 D.1)"
```

---

## Phase E — BlameCache wiring in the Engine

### Task E.1 — Wire `ContentHash` + `BlameCache` into `engine::explain_change`

**Files:**

- Modify: `crates/ohara-engine/src/engine.rs`

- [ ] **Step 1: Write the failing cache-hit test**

Append to the `tests` module at the bottom of
`crates/ohara-engine/src/engine.rs`:

```rust
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn explain_change_blame_cache_hit_on_second_call() {
        // Plan 21 Task E.1: calling explain_change twice for the same
        // file on an unchanged HEAD must result in a BlameCache hit on
        // the second call — i.e., Blamer::blame_range is NOT called a
        // second time. We verify this indirectly by asserting that the
        // BlameCache reports one stored entry for the repo after the
        // first call, and that the second call returns an identical
        // result without error.
        //
        // Analogous to `embed_query_uses_cache_on_repeat_call` (Task B.2
        // in plan-16) and `find_pattern_meta_cached_within_ttl`.
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", ohara_home.path());
        build_test_repo(tmp.path());

        // Index so storage has the commit metadata.
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        {
            let walker = ohara_git::GitWalker::open(&canonical).unwrap();
            let first = walker.first_commit_sha().unwrap();
            let repo_id =
                ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
            let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
            let storage: std::sync::Arc<dyn ohara_core::Storage> =
                std::sync::Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
            let commit_src = ohara_git::GitCommitSource::open(&canonical).unwrap();
            let symbol_src = ohara_parse::GitSymbolSource::open(&canonical).unwrap();
            let indexer = ohara_core::Indexer::new(storage, std::sync::Arc::new(DummyEmbedder));
            indexer
                .run(&repo_id, &commit_src, &symbol_src)
                .await
                .unwrap();
        }

        let engine = make_test_engine();
        let q = ohara_core::explain::ExplainQuery {
            file: "a.rs".into(),
            line_start: 1,
            line_end: 1,
            k: 5,
            include_diff: false,
            include_related: false,
        };

        // First call: cache miss → Blamer runs → result cached.
        let r1 = engine
            .explain_change(&canonical, q.clone())
            .await
            .expect("first explain");

        // Second call: cache hit → Blamer skipped → same result.
        let r2 = engine
            .explain_change(&canonical, q)
            .await
            .expect("second explain");

        // Both calls must produce the same number of hits (single-commit repo).
        assert_eq!(
            r1.hits.len(),
            r2.hits.len(),
            "second call must return same result as first"
        );
        assert_eq!(
            r1.hits.len(),
            1,
            "single-commit repo must produce exactly one blame hit"
        );

        // Expose blame_cache hit count (analogous to meta_hits()).
        // This requires adding a `blame_cache_hits()` accessor to
        // RetrievalEngine — add it in Step 4 below.
        assert_eq!(
            engine.blame_cache_hits(),
            1,
            "second call must increment blame_cache_hits by 1"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-engine explain_change_blame_cache_hit_on_second_call -- --nocapture`
Expected: FAIL — `blame_cache_hits()` method does not exist, and the
engine does not yet check the cache.

- [ ] **Step 3: Commit the red test**

```bash
git add crates/ohara-engine/src/engine.rs
git commit -m "test(engine): plan-21 BlameCache hit on second explain_change (failing)"
```

- [ ] **Step 4: Add `blame_cache_hits` counter and accessor**

In `crates/ohara-engine/src/engine.rs`, add a counter field and
accessor alongside `meta_hit_count`:

```rust
pub struct RetrievalEngine {
    // … existing fields …
    blame_cache_hit_count: AtomicU64,
}
```

In `RetrievalEngine::new`:

```rust
    blame_cache_hit_count: AtomicU64::new(0),
```

Add the accessor (cfg(test) only, matching `meta_hits`):

```rust
    #[cfg(test)]
    pub fn blame_cache_hits(&self) -> u64 {
        self.blame_cache_hit_count.load(Ordering::Relaxed)
    }
```

- [ ] **Step 5: Implement the BlameCache check in `explain_change`**

Replace the current `engine.rs::explain_change` body:

```rust
    pub async fn explain_change(
        &self,
        repo_path: impl AsRef<Path>,
        query: ExplainQuery,
    ) -> crate::Result<ExplainResult> {
        let handle = self.open_repo(repo_path).await?;

        // --- BlameCache wiring (plan-21 E.1) ---
        //
        // Compute the HEAD blob OID for `query.file`. If the file is
        // not present at HEAD (deleted branch, deleted file), fall
        // through to the Blamer which will return its own typed outcome.
        let content_hash_opt: Option<ohara_core::types::ContentHash> =
            compute_head_content_hash(&handle.repo_path, &query.file);

        // Attempt a cache look-up when we have a hash.
        if let Some(ref hash) = content_hash_opt {
            if let Some(cached_ranges) =
                self.blame_cache.get(&handle.repo_id, &query.file, hash.as_str())
            {
                self.blame_cache_hit_count.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    file = %query.file,
                    "explain_change: BlameCache hit, skipping Blamer"
                );
                // Hydrator only — Blamer is bypassed.
                let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
                    &*handle.storage,
                    (*cached_ranges).clone(),
                    &query,
                    &handle.repo_id,
                )
                .await
                .map_err(EngineError::from)?;
                return Ok(assemble_explain_result(hydrated, &query));
            }
        }

        // Cache miss (or file absent from HEAD): run the full path.
        let (hits, meta) = ohara_core::explain::explain_change(
            &*handle.storage,
            &*handle.blamer,
            &handle.repo_id,
            &query,
        )
        .await
        .map_err(EngineError::from)?;

        // Cache the raw blame ranges for next time. Retrieve the ranges
        // from the blamer by re-calling via the orchestrator output — the
        // orchestrator doesn't expose ranges directly yet. To avoid a
        // second Blamer call, cache by re-running blame directly here.
        //
        // Implementation note: once Phase D's orchestrator exposes the
        // raw `Vec<BlameRange>` alongside hits+meta, this can be
        // simplified. For now, call `handle.blamer.blame_range` directly
        // to get the raw ranges for caching.
        if let Some(hash) = content_hash_opt {
            // We already have the orchestrator result; re-blame just for
            // cache population is wasteful. Instead, re-derive the ranges
            // from the hits by inverting the hydration — but hits have
            // already been sorted / truncated to k. The cleanest
            // approach is to expose `Vec<BlameRange>` from the
            // orchestrator.
            //
            // For plan-21, use a different strategy: call
            // `handle.blamer.blame_range` once before the orchestrator
            // in this code path, cache, then pass to a hydration-only
            // path (same as the cache-hit path). This avoids the
            // orchestrator calling blame again internally.
            //
            // Revised structure (replaces the block above):
            use ohara_core::explain::BlameSource;
            let raw_ranges = handle
                .blamer
                .blame_range(&query.file, query.line_start, query.line_end)
                .await
                .map_err(EngineError::from)?;
            let cached = std::sync::Arc::new(raw_ranges.clone());
            self.blame_cache.put(
                handle.repo_id.clone(),
                query.file.clone(),
                hash.as_str().to_string(),
                cached,
            );
            let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
                &*handle.storage,
                raw_ranges,
                &query,
                &handle.repo_id,
            )
            .await
            .map_err(EngineError::from)?;
            return Ok(assemble_explain_result(hydrated, &query));
        }

        Ok(ExplainResult { hits, meta })
    }
```

Note: the above structure has a logic overlap. The final implementation
MUST be written as a single clean flow to avoid calling the blamer twice.
The clean version is:

```rust
    pub async fn explain_change(
        &self,
        repo_path: impl AsRef<Path>,
        query: ExplainQuery,
    ) -> crate::Result<ExplainResult> {
        let handle = self.open_repo(repo_path).await?;
        let content_hash_opt = compute_head_content_hash(&handle.repo_path, &query.file);

        // Cache hit path.
        if let Some(ref hash) = content_hash_opt {
            if let Some(cached) =
                self.blame_cache.get(&handle.repo_id, &query.file, hash.as_str())
            {
                self.blame_cache_hit_count.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(file = %query.file, "explain_change: BlameCache hit");
                let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
                    &*handle.storage,
                    (*cached).clone(),
                    &query,
                    &handle.repo_id,
                )
                .await
                .map_err(EngineError::from)?;
                return Ok(assemble_explain_result(hydrated, &query));
            }
        }

        // Cache miss: blame → cache → hydrate.
        use ohara_core::explain::BlameSource;
        let raw_ranges = handle
            .blamer
            .blame_range(&query.file, query.line_start, query.line_end)
            .await
            .map_err(EngineError::from)?;

        if let Some(hash) = content_hash_opt {
            self.blame_cache.put(
                handle.repo_id.clone(),
                query.file.clone(),
                hash.as_str().to_string(),
                std::sync::Arc::new(raw_ranges.clone()),
            );
        }

        let hydrated = ohara_core::explain::hydrator::hydrate_blame_results(
            &*handle.storage,
            raw_ranges,
            &query,
            &handle.repo_id,
        )
        .await
        .map_err(EngineError::from)?;
        Ok(assemble_explain_result(hydrated, &query))
    }
```

Add the two free functions at the bottom of `engine.rs` (above the
`tests` module):

```rust
/// Compute the git blob OID for `file` at HEAD in `repo_path`.
///
/// Returns `None` when the file is absent from HEAD (deleted file,
/// wrong path) or when the git2 repository can't be opened. Callers
/// treat `None` as "skip the cache, let the Blamer decide".
fn compute_head_content_hash(
    repo_path: &std::path::Path,
    file: &str,
) -> Option<ohara_core::types::ContentHash> {
    let repo = git2::Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(file)).ok()?;
    Some(ohara_core::types::ContentHash::from_blob_oid(entry.id()))
}

/// Assemble `ExplainResult` from a `HydratedBlame` + the original query.
///
/// Applies the same sort+truncate that the orchestrator applies. Used
/// by both the cache-hit and cache-miss paths so the logic lives in
/// one place.
fn assemble_explain_result(
    mut hydrated: ohara_core::explain::hydrator::HydratedBlame,
    query: &ohara_core::explain::ExplainQuery,
) -> ExplainResult {
    const K_MAX: u8 = 20;
    hydrated.hits.sort_by(|a, b| match b.commit_date.cmp(&a.commit_date) {
        std::cmp::Ordering::Equal => a.commit_sha.cmp(&b.commit_sha),
        other => other,
    });
    let k = query.k.clamp(1, K_MAX) as usize;
    hydrated.hits.truncate(k);
    ExplainResult {
        hits: hydrated.hits,
        meta: ohara_core::explain::ExplainMeta {
            lines_queried: hydrated.clamped_range,
            commits_unique: hydrated.hits.len(),
            blame_coverage: hydrated.coverage,
            limitation: hydrated.limitation,
            related_commits: hydrated.related_commits,
            enrichment_limitation: hydrated.enrichment_limitation,
        },
    }
}
```

Note: `hydrated.hits` is moved into `ExplainResult`; `commits_unique`
must be set before the move. Adjust field order:

```rust
    let commits_unique = hydrated.hits.len();
    ExplainResult {
        hits: hydrated.hits,
        meta: ohara_core::explain::ExplainMeta {
            lines_queried: hydrated.clamped_range,
            commits_unique,
            blame_coverage: hydrated.coverage,
            limitation: hydrated.limitation,
            related_commits: hydrated.related_commits,
            enrichment_limitation: hydrated.enrichment_limitation,
        },
    }
```

- [ ] **Step 6: Make `hydrate_blame_results` and `HydratedBlame` pub-accessible from ohara-engine**

`ohara-engine` depends on `ohara-core`. The hydrator module is
`pub(crate)` inside `ohara-core`. Make it `pub` so the engine can
call it:

In `crates/ohara-core/src/explain/mod.rs`:

```rust
pub mod hydrator;
```

In `crates/ohara-core/src/lib.rs` (or `explain/mod.rs` re-exports),
add a re-export so the engine can import
`ohara_core::explain::hydrator::hydrate_blame_results` and
`ohara_core::explain::hydrator::HydratedBlame`.

Also ensure `ohara-engine/Cargo.toml` already depends on `ohara-core`
(it does — check `crates/ohara-engine/Cargo.toml`).

- [ ] **Step 7: Run the cache-hit test to verify it passes**

Run: `cargo test -p ohara-engine explain_change_blame_cache_hit_on_second_call -- --nocapture`
Expected: PASS.

- [ ] **Step 8: Run the full engine suite**

Run: `cargo test -p ohara-engine`
Expected: PASS — existing `explain_change_returns_one_blame_range_for_single_commit_repo`
and all other engine tests still green.

- [ ] **Step 9: Run the MCP envelope-parity goldens**

Run: `cargo test -p ohara-mcp`
Expected: PASS — JSON envelope shape unchanged.

- [ ] **Step 10: Commit**

```bash
git add crates/ohara-engine/src/engine.rs crates/ohara-core/src/explain/
git commit -m "feat(engine): wire BlameCache in explain_change via ContentHash HEAD blob OID (plan-21 E.1)"
```

---

## Phase F — Cleanup and verification

### Task F.1 — Remove dead code, check line counts, run full suite

**Files:**

- Modify: `crates/ohara-core/src/explain/mod.rs` (remove inline
  helpers if any remain)
- Modify: `CONTRIBUTING.md` (workspace-layout table, if
  `explain/` sub-module structure changed from a flat file)

- [ ] **Step 1: Confirm no dead code warnings**

Run: `cargo build --workspace --all-targets --all-features 2>&1 | grep -i "dead_code\|unused"`
Expected: no warnings about `explain.rs` helpers.

- [ ] **Step 2: Check all file sizes**

Run:
```bash
wc -l crates/ohara-core/src/explain/mod.rs \
       crates/ohara-core/src/explain/hydrator.rs \
       crates/ohara-engine/src/engine.rs
```
Expected: each file under 500 lines. If `engine.rs` exceeds 500,
extract `compute_head_content_hash` and `assemble_explain_result` into
a sibling `crates/ohara-engine/src/explain_helpers.rs` and `pub(crate)
mod explain_helpers;` from `lib.rs`.

- [ ] **Step 3: Update `CONTRIBUTING.md` workspace-layout table**

Find the `## Workspace layout` table in `CONTRIBUTING.md`; if the
`ohara-core` row doesn't mention `explain/` as a sub-module directory,
update it:

```markdown
| `ohara-core` | Domain types, traits, orchestration (`Indexer`, `Retriever`, `ExplainQuery`); `explain/hydrator.rs` for independently-testable blame hydration |
```

- [ ] **Step 4: Run the full pre-completion checklist**

See § "Pre-completion checklist" below.

- [ ] **Step 5: Commit**

```bash
git add CONTRIBUTING.md crates/ohara-core/ crates/ohara-engine/
git commit -m "chore(core): plan-21 cleanup — remove dead helpers, update layout table"
```

---

## Pre-completion checklist (CONTRIBUTING.md §13)

Before opening a PR, run each item; all must be green:

- [ ] `cargo fmt --all` — clean
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` — zero warnings
- [ ] `cargo test --workspace` — all tests green
- [ ] `cargo test -p ohara-mcp` — `envelope_parity` golden tests pass
      byte-identically (JSON shape of `explain_change` response unchanged)
- [ ] `wc -l crates/ohara-core/src/explain/mod.rs` — under 500 lines
- [ ] `wc -l crates/ohara-core/src/explain/hydrator.rs` — under 500 lines
- [ ] `wc -l crates/ohara-engine/src/engine.rs` — under 500 lines
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code outside
      the `expect("invariant: …")` exemption
- [ ] No `println!` / `eprintln!` outside `ohara-cli` and `tests/perf`
- [ ] No new top-level `*.md` files
- [ ] All SQL stays in `ohara-storage` — no SQL strings in `ohara-core`
      or `ohara-engine`
- [ ] `ohara-core` has no dependency on `ohara-storage`, `ohara-embed`,
      `ohara-git`, or `ohara-parse` (verify `Cargo.toml` deps)
- [ ] `git2` dep on `ohara-core` verified not to create a circular dep
      (ohara-git already depends on ohara-core; ohara-core depending on
      git2 the crate is fine — git2 is not an ohara crate)

---

## Risks and decisions

**`tree.get_path` returns `NotFound` when file is absent from HEAD.**
Decided in Phase E: `compute_head_content_hash` returns `None` on any
git2 error including NotFound. The engine's cache path is skipped;
`Blamer::blame_range` runs normally and returns its own "file not found"
outcome. No special-casing required in the engine.

**Default-loop `get_commits_by_sha` is slower for `MockStorage` in
tests.** Accepted: test fakes call `get_commit` N times via the default.
For the test batch sizes (2–5 SHAs) the cost is negligible.
`SqliteStorage`'s real impl issues one SQL statement regardless of N.

**`ohara-core` now depends on `git2`.** Verified: `git2` is a
third-party foundation library, not an ohara crate. The crate dependency
direction rule (`ohara-core` MUST NOT depend on `ohara-storage` /
`-embed` / `-git` / `-parse`) is about ohara's own crates. A `git2`
dep is already used transitively and adding it as a direct dep to
`ohara-core` does not introduce a cycle.

**`HydratedBlame` visibility.** The struct must be `pub` (not
`pub(crate)`) because `ohara-engine` is a separate crate that calls
`hydrate_blame_results`. The `pub mod hydrator;` in `explain/mod.rs`
makes the module accessible; `pub struct HydratedBlame` and
`pub async fn hydrate_blame_results` are the only items that need to
be exported. Helper functions `build_limitation` and
`collect_related_commits` remain `pub(crate)`.

**`K_MAX` constant duplication.** After Phase D the constant lives in
`explain/mod.rs` for the orchestrator and is re-used in
`assemble_explain_result` in `ohara-engine`. Either re-export it from
`ohara-core::explain` or duplicate the value (20) in the engine as a
private constant. Duplication is simpler; add a comment linking to
the core definition.
