# ohara plan-24 — Lane-mask hoist + batched symbol hydration

> **Status:** complete

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each green
> implementation.

**Goal:** turn ohara's existing
`RetrievalProfile::{vec,text,symbol}_lane_enabled` flags into actual
work-skipping gates instead of post-hoc filters, and replace the N+1
per-hit `Storage::get_hunk_symbols` loop in
`Retriever::find_pattern_with_profile` with a single
`get_hunk_symbols_batch` call. Both changes are pure latency wins; no
ranking semantics change.

**Architecture:**

1. **Lane-mask hoist.** Today
   `Retriever::find_pattern_with_profile` (`crates/ohara-core/src/retriever.rs:155`)
   `tokio::join!`s all four lane futures unconditionally and only
   discards results based on the profile flags
   (`retriever.rs:197-215`). Profiles like `bug_fix` /
   `configuration` therefore pay the full retrieval cost for
   behavioural-only differences. We hoist the gates above `join!` so
   disabled lanes never run their SQL / embed call. The vec lane's
   pre-step (`embedder.embed_batch`) is also skipped when
   `vec_lane_enabled = false`. We use `OptionFuture` (i.e.
   `Option<Future>` joined via `futures::future::join`) so disabled
   lanes resolve to `None` cleanly.

2. **Batched symbol hydration.** Today the symbol-attribution
   hydration step (`retriever.rs:286-298`) calls
   `Storage::get_hunk_symbols(repo_id, hunk_id)` once per surviving
   hit — ≤20 sequential SQL round-trips. We add a
   `Storage::get_hunk_symbols_batch(repo_id, &[HunkId])` method (one
   `IN (?,…)` query — same shape as the existing
   `get_commits_by_sha` introduced in plan-21) and replace the loop
   with one call.

**Tech Stack:** Rust 2021, `futures::future::OptionFuture`,
`async-trait`, `rusqlite` (matches existing storage patterns), no new
crates.

**Spec:** none — internal performance / correctness cleanup.

**Scope check:** plan-24 touches `ohara-core` (retriever + storage
trait), `ohara-storage` (new query helper + impl), and the in-tree
storage fakes inside `ohara-core` retriever tests. No SQL migration
(read-only addition). No CLI / MCP behavior change. No new public
API surface beyond the storage-trait method.

---

## Phase A — Storage: `get_hunk_symbols_batch`

### Task A.1 — Failing test for the table-level helper

**Files:**
- Modify: `crates/ohara-storage/src/tables/hunk_symbol.rs` (add a
  `get_for_hunks` function and a unit test)

- [ ] **Step 1: Read the existing `get_for_hunk` function**

Run: `grep -n "pub fn get_for_hunk\b" crates/ohara-storage/src/tables/hunk_symbol.rs`

Expected: one match around line 158. Open it. The new batch function
mirrors its row-decode logic but builds an `IN (?,…)` clause.

- [ ] **Step 2: Read the existing batch-fetch precedent**

Run: `grep -n "fn get_by_shas\|placeholders\|IN (\\{placeholders\\})" crates/ohara-storage/src/tables/commit.rs crates/ohara-storage/src/storage_impl.rs`

Expected: `get_commits_by_sha` (plan-21) shows the exact placeholder
construction style this codebase uses. Mirror it.

- [ ] **Step 3: Add a failing test in `hunk_symbol.rs`**

At the bottom of `crates/ohara-storage/src/tables/hunk_symbol.rs`
inside the existing `#[cfg(test)] mod tests` block (or create one if
none exists), add:

```rust
#[cfg(test)]
mod batch_tests {
    use super::*;
    use ohara_core::types::{AttributionKind, HunkSymbol, SymbolKind};
    use rusqlite::Connection;

    /// Build an in-memory schema with just the columns `hunk_symbol`
    /// needs. The `hunk` and `commit_record` foreign-key targets are
    /// stubbed because `get_for_hunks` does not join.
    fn schema(c: &Connection) {
        c.execute_batch(
            "CREATE TABLE hunk_symbol (
                hunk_id          INTEGER NOT NULL,
                symbol_kind      TEXT NOT NULL,
                symbol_name      TEXT NOT NULL,
                qualified_name   TEXT,
                attribution_kind TEXT NOT NULL
            );",
        )
        .unwrap();
    }

    fn insert(c: &Connection, hunk_id: i64, name: &str, kind: AttributionKind) {
        c.execute(
            "INSERT INTO hunk_symbol \
                 (hunk_id, symbol_kind, symbol_name, qualified_name, attribution_kind) \
             VALUES (?1, 'function', ?2, NULL, ?3)",
            rusqlite::params![hunk_id, name, kind.as_str()],
        )
        .unwrap();
    }

    #[test]
    fn get_for_hunks_groups_results_by_hunk_id_in_a_single_query() {
        let c = Connection::open_in_memory().unwrap();
        schema(&c);
        insert(&c, 10, "alpha", AttributionKind::ExactSpan);
        insert(&c, 10, "beta", AttributionKind::HunkHeader);
        insert(&c, 11, "gamma", AttributionKind::ExactSpan);
        // 12 has no rows — must appear as an empty Vec, not be missing.

        let got = get_for_hunks(&c, &[10_i64, 11, 12]).unwrap();
        assert_eq!(got.len(), 3, "every requested hunk_id must be represented");

        let h10: &Vec<HunkSymbol> = got.get(&10).expect("hunk 10");
        assert_eq!(h10.len(), 2);
        // Plan 11 ordering: ExactSpan before HunkHeader, then symbol_name ASC.
        assert_eq!(h10[0].name, "alpha");
        assert_eq!(h10[0].attribution, AttributionKind::ExactSpan);
        assert_eq!(h10[1].name, "beta");
        assert_eq!(h10[1].attribution, AttributionKind::HunkHeader);

        let h11: &Vec<HunkSymbol> = got.get(&11).expect("hunk 11");
        assert_eq!(h11.len(), 1);
        assert_eq!(h11[0].name, "gamma");

        let h12: &Vec<HunkSymbol> = got.get(&12).expect("hunk 12");
        assert!(h12.is_empty(), "no rows ⇒ empty Vec, never missing");
    }

    #[test]
    fn get_for_hunks_returns_empty_map_for_empty_input() {
        let c = Connection::open_in_memory().unwrap();
        schema(&c);
        let got = get_for_hunks(&c, &[]).unwrap();
        assert!(got.is_empty(), "empty input ⇒ empty map, no SQL roundtrip");
    }
}
```

- [ ] **Step 4: Run the test and confirm it fails**

Run: `cargo test -p ohara-storage --lib hunk_symbol::batch_tests`

Expected: fails — `get_for_hunks` is not yet defined.

- [ ] **Step 5: Commit the failing test**

```bash
git add crates/ohara-storage/src/tables/hunk_symbol.rs
git commit -m "test(storage): add failing batch-fetch tests for hunk_symbol::get_for_hunks"
```

### Task A.2 — Implement `get_for_hunks`

**Files:**
- Modify: `crates/ohara-storage/src/tables/hunk_symbol.rs`

- [ ] **Step 1: Add the function**

Insert after `get_for_hunk` in `hunk_symbol.rs`:

```rust
/// Plan 24 batch variant of `get_for_hunk`. Returns a map keyed by the
/// requested `hunk_id`s; every requested id is present in the map (as
/// an empty `Vec` when the hunk has no attribution rows). Ordering of
/// each per-hunk `Vec` matches `get_for_hunk`: ExactSpan before
/// HunkHeader before everything else, then `symbol_name` ASC.
pub fn get_for_hunks(
    c: &Connection,
    hunk_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<HunkSymbol>>> {
    if hunk_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    // Seed every requested id with an empty Vec so callers can rely on
    // "every id is present in the map" — matches the contract documented
    // on the function.
    let mut acc: std::collections::HashMap<i64, Vec<HunkSymbol>> =
        hunk_ids.iter().map(|id| (*id, Vec::new())).collect();

    let placeholders = hunk_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT hunk_id, symbol_kind, symbol_name, qualified_name, attribution_kind \
         FROM hunk_symbol \
         WHERE hunk_id IN ({placeholders}) \
         ORDER BY hunk_id ASC, \
           CASE attribution_kind \
             WHEN 'exact_span' THEN 0 \
             WHEN 'hunk_header' THEN 1 \
             ELSE 2 \
           END ASC, symbol_name ASC"
    );
    let mut stmt = c.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = hunk_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        let hunk_id: i64 = row.get(0)?;
        let kind_s: String = row.get(1)?;
        let name: String = row.get(2)?;
        let qualified_name: Option<String> = row.get(3)?;
        let attribution_s: String = row.get(4)?;
        let kind = str_to_symbol_kind(&kind_s).unwrap_or(SymbolKind::Function);
        let attribution =
            AttributionKind::from_str(&attribution_s).unwrap_or(AttributionKind::HunkHeader);
        Ok((
            hunk_id,
            HunkSymbol {
                kind,
                name,
                qualified_name,
                attribution,
            },
        ))
    })?;
    for r in rows {
        let (hunk_id, sym) = r?;
        acc.entry(hunk_id).or_insert_with(Vec::new).push(sym);
    }
    Ok(acc)
}
```

- [ ] **Step 2: Run the test and confirm it passes**

Run: `cargo test -p ohara-storage --lib hunk_symbol::batch_tests`

Expected: PASS (both tests).

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-storage/src/tables/hunk_symbol.rs
git commit -m "feat(storage): add hunk_symbol::get_for_hunks batch fetcher"
```

### Task A.3 — Wire `get_hunk_symbols_batch` through the `Storage` trait

**Files:**
- Modify: `crates/ohara-core/src/storage.rs:248-251` (trait method)
- Modify: `crates/ohara-core/src/storage.rs:35-45` (counter struct,
  if visible — match existing `get_hunk_symbols` counter pattern)
- Modify: `crates/ohara-storage/src/storage_impl.rs:226-238` (impl)
- Modify: `crates/ohara-core/src/retriever.rs` (test fakes — see step 5)
- Modify: `crates/ohara-core/src/explain/tests.rs` (any other fakes)

- [ ] **Step 1: Add the trait method (failing-build state)**

In `crates/ohara-core/src/storage.rs`, immediately after the existing
`get_hunk_symbols` declaration, add:

```rust
    /// Plan 24 batch variant. Same contract as `get_hunk_symbols` but
    /// for many hunks at once. Implementations MUST return a map with
    /// every requested `hunk_id` present (empty `Vec` when no
    /// attribution rows exist).
    async fn get_hunk_symbols_batch(
        &self,
        repo_id: &RepoId,
        hunk_ids: &[HunkId],
    ) -> Result<std::collections::HashMap<HunkId, Vec<HunkSymbol>>>;
```

- [ ] **Step 2: Add the storage-counter field (if a struct exists)**

If the metrics struct around `crates/ohara-core/src/storage.rs:35-45`
has a `pub get_hunk_symbols: StorageMethodMetrics` field, add the
matching:

```rust
    pub get_hunk_symbols_batch: StorageMethodMetrics,
```

If `StorageMethodMetrics` has a `Default` impl this is the only
edit; otherwise also extend the struct's manual constructor.

- [ ] **Step 3: Run `cargo build` to surface every fake that needs updating**

Run: `cargo build --workspace`

Expected: errors of the form `not all trait items implemented` for
every type that `impl Storage for …`. Note them — Step 5 fixes them.

- [ ] **Step 4: Implement `get_hunk_symbols_batch` in `SqliteStorage`**

In `crates/ohara-storage/src/storage_impl.rs`, immediately after the
existing `get_hunk_symbols` impl, add:

```rust
    async fn get_hunk_symbols_batch(
        &self,
        _repo_id: &RepoId,
        hunk_ids: &[HunkId],
    ) -> CoreResult<std::collections::HashMap<HunkId, Vec<HunkSymbol>>> {
        if hunk_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let ids_owned: Vec<HunkId> = hunk_ids.to_vec();
        timed_with_conn(
            &self.pool,
            &self.counters.get_hunk_symbols_batch,
            |m: &std::collections::HashMap<HunkId, Vec<HunkSymbol>>| {
                m.values().map(|v| v.len() as u64).sum()
            },
            move |c| {
                // The table-level helper takes raw i64; HunkId derefs
                // / converts via .0 — match whatever the codebase uses
                // elsewhere when crossing the boundary.
                let raw: Vec<i64> = ids_owned.iter().map(|id| id.0).collect();
                let map = crate::tables::hunk_symbol::get_for_hunks(c, &raw)?;
                // Re-key the result with the strong HunkId newtype.
                Ok(map.into_iter().map(|(k, v)| (HunkId(k), v)).collect())
            },
        )
        .await
    }
```

> **Note on `HunkId.0`:** if `HunkId` is a struct-tuple newtype (most
> likely given CONTRIBUTING.md §2 "Newtypes for meaningful primitives"),
> `.0` extracts the inner `i64`. If it's an enum or has a different
> field name, mirror whatever existing `get_hunk_symbols` does to
> bridge the type.

- [ ] **Step 5: Update each test fake found in Step 3**

For every type the build flagged as not implementing the new method,
add a default impl that returns an empty map (the existing
`get_hunk_symbols` fakes return `Vec::new()` — same approach):

```rust
        async fn get_hunk_symbols_batch(
            &self,
            _: &RepoId,
            _: &[HunkId],
        ) -> crate::Result<std::collections::HashMap<HunkId, Vec<crate::types::HunkSymbol>>> {
            Ok(std::collections::HashMap::new())
        }
```

Likely locations (from a global search):

- `crates/ohara-core/src/retriever.rs` — `FakeStorage` (around the
  test block, near line 421+).
- `crates/ohara-core/src/explain/tests.rs` — explain-side test storage
  fake, if one exists.
- Any other `impl Storage for` in the workspace surfaced by Step 3.

- [ ] **Step 6: Build and run the full unit-test suite**

Run: `cargo build --workspace`
Run: `cargo test --workspace --lib`

Expected: build clean, every test passes (the new method is unused so
far; it's just plumbed).

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/storage.rs \
        crates/ohara-storage/src/storage_impl.rs \
        crates/ohara-core/src/retriever.rs \
        crates/ohara-core/src/explain/tests.rs
git commit -m "feat(storage): plumb get_hunk_symbols_batch through Storage trait"
```

> If only some of the listed paths exist, just stage the ones you actually
> changed.

---

## Phase B — Retriever: replace N+1 hydration loop

### Task B.1 — Failing test that pins one batch call

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (test block)

- [ ] **Step 1: Extend `FakeStorage` with a batch-call counter**

Inside the existing `FakeStorage` definition (test module of
`retriever.rs`), add a new field:

```rust
        batch_calls: Mutex<usize>,
```

…and initialize it to 0 in `FakeStorage::new`. Every existing test
that constructs `FakeStorage::new(...)` keeps working (the field is
internal and defaulted).

Then update the trait impl (the empty `get_hunk_symbols_batch`
inserted in Phase A Task A.3) to increment the counter before
returning:

```rust
        async fn get_hunk_symbols_batch(
            &self,
            _: &RepoId,
            _: &[HunkId],
        ) -> crate::Result<std::collections::HashMap<HunkId, Vec<crate::types::HunkSymbol>>> {
            *self.batch_calls.lock().unwrap() += 1;
            Ok(std::collections::HashMap::new())
        }
```

- [ ] **Step 2: Add a regression test that asserts a single batch call**

Append to the `mod tests` block of `retriever.rs`:

```rust
#[tokio::test]
async fn find_pattern_calls_get_hunk_symbols_batch_exactly_once() {
    // Plan 24 regression: hydration must be one batch call, not N
    // sequential calls. We construct lanes that surface 5 distinct
    // hunks; the retriever should make exactly 1 call to the batch
    // method and 0 calls to the per-hit method.
    let now = 1_700_000_000;
    let knn = vec![
        fake_hit(1, "a", now, 0.9, "diff-a"),
        fake_hit(2, "b", now, 0.5, "diff-b"),
        fake_hit(3, "c", now, 0.4, "diff-c"),
        fake_hit(4, "d", now, 0.3, "diff-d"),
        fake_hit(5, "e", now, 0.2, "diff-e"),
    ];
    let storage = Arc::new(FakeStorage::new(knn, vec![], vec![]));
    let embedder = Arc::new(FakeEmbedder);
    let r = Retriever::new(storage.clone(), embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let _ = r.find_pattern(&id, &q, now).await.unwrap();

    let batch_calls = *storage.batch_calls.lock().unwrap();
    assert_eq!(
        batch_calls, 1,
        "hydrate_symbols MUST issue exactly 1 batch call for ≥1 surviving hits, got {batch_calls}"
    );

    // The per-hit method must NOT have been called by the retriever.
    let per_hit_calls = storage
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == "get_hunk_symbols")
        .count();
    assert_eq!(
        per_hit_calls, 0,
        "per-hit get_hunk_symbols must not be called by the retriever after plan-24"
    );
}
```

- [ ] **Step 3: Update the existing `get_hunk_symbols` fake to record its call**

In `FakeStorage`'s impl of `get_hunk_symbols`, push `"get_hunk_symbols"`
into `self.calls` before returning. (Currently it returns `Vec::new()`
without recording — the new test inspects this.)

- [ ] **Step 4: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib find_pattern_calls_get_hunk_symbols_batch_exactly_once -- --nocapture`

Expected: fails with `batch_calls == 0` (the retriever still uses the
per-hit loop).

- [ ] **Step 5: Commit the failing test**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "test(retriever): add failing assertion for single batch hydration call"
```

### Task B.2 — Replace the loop with one batch call

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs:286-298`

- [ ] **Step 1: Replace the hydration block**

Replace the `symbols_by_hunk` construction in
`find_pattern_with_profile` (currently `retriever.rs:286-298`) with:

```rust
        // Plan 24: one batch call rather than N sequential per-hit
        // round-trips. Storage seeds every requested hunk_id in the
        // returned map (with an empty Vec when no attribution rows
        // exist), so the subsequent `.get(&id).cloned().unwrap_or_default()`
        // call below remains correct without further branching.
        let hunk_ids: Vec<HunkId> = hits.iter().map(|h| h.hunk_id).collect();
        let symbols_by_hunk: std::collections::HashMap<HunkId, Vec<String>> =
            timed_phase("hydrate_symbols", async {
                let attrs_map = self
                    .storage
                    .get_hunk_symbols_batch(repo_id, &hunk_ids)
                    .await?;
                Ok::<_, crate::OhraError>(
                    attrs_map
                        .into_iter()
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(id, v)| (id, v.into_iter().map(|a| a.name).collect()))
                        .collect(),
                )
            })
            .await?;
```

- [ ] **Step 2: Run the regression test and confirm it passes**

Run: `cargo test -p ohara-core --lib find_pattern_calls_get_hunk_symbols_batch_exactly_once`

Expected: PASS.

- [ ] **Step 3: Run the full retriever test suite**

Run: `cargo test -p ohara-core --lib retriever::`

Expected: every test passes — the per-hit loop's external behavior
(returning `related_head_symbols` on each `PatternHit`) is preserved
exactly when the storage backend honors the batch contract.

- [ ] **Step 4: Commit**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "perf(retriever): batch get_hunk_symbols call (was N+1 sequential)"
```

---

## Phase C — Hoist lane-mask gates above `tokio::join!`

### Task C.1 — Failing test: disabled lanes do not hit the storage backend

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (test block)

- [ ] **Step 1: Add a profile-injection seam to `Retriever`**

The existing public API derives the profile from the query string,
which makes it inconvenient to test the lane-mask paths. Add an
internal-only override that the test can use:

```rust
impl Retriever {
    /// Test-only: skip query parsing and use this profile verbatim.
    /// Production callers go through `find_pattern` /
    /// `find_pattern_with_profile`, which derive the profile from the
    /// query.
    #[cfg(test)]
    pub(crate) async fn find_pattern_with_explicit_profile(
        &self,
        repo_id: &crate::types::RepoId,
        query: &PatternQuery,
        profile: crate::query_understanding::RetrievalProfile,
        now_unix: i64,
    ) -> crate::Result<Vec<PatternHit>> {
        // Body identical to find_pattern_with_profile but with the
        // `parse_query` call replaced by `profile` — see Task C.2 for
        // the shared implementation.
        let (hits, _) = self
            .find_pattern_inner(repo_id, query, profile, now_unix)
            .await?;
        Ok(hits)
    }
}
```

(Task C.2 introduces `find_pattern_inner` as the shared body.)

- [ ] **Step 2: Add the failing test**

```rust
#[tokio::test]
async fn disabled_lanes_skip_storage_calls() {
    // Plan 24: when the profile disables a lane, the corresponding
    // storage method (or embedding call, for the vec lane) MUST NOT
    // run. Pre-fix the retriever calls all four lanes unconditionally
    // and only filters the results post-hoc.
    let now = 1_700_000_000;
    let storage = Arc::new(FakeStorage::new(vec![], vec![], vec![]));
    let embedder = Arc::new(CountingEmbedder::default());
    let r = Retriever::new(storage.clone(), embedder.clone());

    // Profile: only the text lane is enabled.
    let mut profile = crate::query_understanding::RetrievalProfile::default_unknown();
    profile.vec_lane_enabled = false;
    profile.symbol_lane_enabled = false;
    // text_lane_enabled stays true.

    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let _ = r
        .find_pattern_with_explicit_profile(&id, &q, profile, now)
        .await
        .unwrap();

    let calls = storage.calls.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|c| *c == "knn"),
        "vec lane disabled: knn_hunks must not run; calls = {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| *c == "fts_sym" || *c == "fts_hist_sym"),
        "symbol lane disabled: fts_sym / fts_hist_sym must not run; calls = {calls:?}"
    );
    assert!(
        calls.iter().any(|c| *c == "fts_text"),
        "text lane enabled: fts_text MUST run; calls = {calls:?}"
    );

    let embed_calls = embedder.calls();
    assert_eq!(
        embed_calls, 0,
        "vec lane disabled: embed_batch must not be called for the query; got {embed_calls}"
    );
}
```

- [ ] **Step 3: Add the `CountingEmbedder` test fake near `FakeEmbedder`**

```rust
    #[derive(Default)]
    struct CountingEmbedder {
        calls: Mutex<usize>,
    }

    impl CountingEmbedder {
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl crate::EmbeddingProvider for CountingEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "counting"
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            *self.calls.lock().unwrap() += 1;
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }
```

- [ ] **Step 4: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib disabled_lanes_skip_storage_calls -- --nocapture`

Expected: fails with at least one of:
- `vec lane disabled: knn_hunks must not run; calls = ["knn", "fts_text", "fts_sym", "fts_hist_sym"]`
- `embed_batch must not be called for the query; got 1`

- [ ] **Step 5: Commit the failing test**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "test(retriever): assert disabled lanes skip storage + embed calls"
```

### Task C.2 — Hoist the gates: extract `find_pattern_inner` and gate-before-join

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (~lines 107-347)

- [ ] **Step 1: Extract a shared inner method**

Replace the existing `find_pattern_with_profile` body (everything
after the local-variable setup that derives `parsed`, `profile`,
`effective_language`, `effective_weights`, `q_text`) with a call
to a new private async method `find_pattern_inner` that takes the
already-resolved profile + effective_weights as arguments. The
public `find_pattern_with_profile` remains a thin wrapper that does
the parse-then-call.

The factoring keeps:
- `find_pattern` → `find_pattern_with_profile` (public, unchanged
  signature)
- `find_pattern_with_profile` → derives profile, calls inner
- `find_pattern_with_explicit_profile` (test-only) → bypasses derive,
  calls inner
- `find_pattern_inner` → the actual pipeline (lane gating, RRF,
  rerank, recency, hydration)

- [ ] **Step 2: Replace the unconditional `tokio::join!` with gated execution**

Inside `find_pattern_inner`, replace the lane-gather block (currently
`retriever.rs:137-225`) with the following. The key shape change:
each lane is wrapped in `OptionFuture` so disabled lanes resolve to
`None` without ever spawning the work; the embedding call moves
inside the vec-lane branch.

```rust
        use futures::future::OptionFuture;

        let language_filter = effective_language.as_deref();
        let since_unix = query.since_unix.or(parsed_since_unix);

        // Vec lane: only embed if the lane is enabled. The embedding
        // call is the single biggest setup cost when the vec lane is
        // off, so this is the headline saving.
        let vec_fut: OptionFuture<_> = if profile.vec_lane_enabled {
            let q_text = vec![query.query.clone()];
            let storage = self.storage.clone();
            let embedder = self.embedder.clone();
            let lane_top_k = effective_weights.lane_top_k;
            let lang = language_filter.map(|s| s.to_string());
            Some(timed_phase("lane_knn", async move {
                let mut q_embs = embedder.embed_batch(&q_text).await?;
                let q_emb = q_embs.pop().ok_or_else(|| {
                    crate::OhraError::Embedding("empty".into())
                })?;
                storage
                    .knn_hunks(repo_id, &q_emb, lane_top_k, lang.as_deref(), since_unix)
                    .await
            }))
            .into()
        } else {
            None.into()
        };

        let fts_fut: OptionFuture<_> = if profile.text_lane_enabled {
            Some(timed_phase(
                "lane_fts_text",
                self.storage.bm25_hunks_by_text(
                    repo_id,
                    &query.query,
                    effective_weights.lane_top_k,
                    language_filter,
                    since_unix,
                ),
            ))
            .into()
        } else {
            None.into()
        };

        let (hist_sym_fut, head_sym_fut): (OptionFuture<_>, OptionFuture<_>) =
            if profile.symbol_lane_enabled {
                (
                    Some(timed_phase(
                        "lane_fts_sym_hist",
                        self.storage.bm25_hunks_by_historical_symbol(
                            repo_id,
                            &query.query,
                            effective_weights.lane_top_k,
                            language_filter,
                            since_unix,
                        ),
                    ))
                    .into(),
                    Some(timed_phase(
                        "lane_fts_sym_head",
                        self.storage.bm25_hunks_by_symbol_name(
                            repo_id,
                            &query.query,
                            effective_weights.lane_top_k,
                            language_filter,
                            since_unix,
                        ),
                    ))
                    .into(),
                )
            } else {
                (None.into(), None.into())
            };

        let (vec_opt, fts_opt, hist_sym_opt, head_sym_opt) =
            tokio::join!(vec_fut, fts_fut, hist_sym_fut, head_sym_fut);

        // Each `_opt` is `Option<Result<Vec<HunkHit>>>`. None ⇒ lane
        // disabled (skip silently); Some(Err) ⇒ lane errored (propagate);
        // Some(Ok) ⇒ use the hits. Disabled lanes contribute Vec::new().
        let vec_hits: Vec<HunkHit> = vec_opt.transpose()?.unwrap_or_default();
        let fts_hits: Vec<HunkHit> = fts_opt.transpose()?.unwrap_or_default();
        let hist_sym_hits: Vec<HunkHit> = hist_sym_opt.transpose()?.unwrap_or_default();
        let head_sym_hits: Vec<HunkHit> = head_sym_opt.transpose()?.unwrap_or_default();

        // Plan 11 Task 4.1 Step 3 still applies post-hoist: prefer
        // historical attribution when present, fall back to HEAD-symbol.
        let sym_hits = if hist_sym_hits.is_empty() {
            head_sym_hits
        } else {
            hist_sym_hits
        };
```

> **Note on `parsed_since_unix`:** the original code calls
> `query.since_unix.or(parsed.since_unix)` four times. After the
> hoist, capture the resolved value into a single `since_unix: Option<i64>`
> binding above the lane futures (the snippet above assumes this). The
> `parsed` variable comes from `query_understanding::parse_query` in
> `find_pattern_with_profile`; the test-only entry-point doesn't run
> the parser, so it must pass `parsed.since_unix = None` (or whatever
> the explicit-profile signature decides). Keep the override
> path's behavior consistent — the `find_pattern_with_explicit_profile`
> test signature should accept an `Option<i64> for since_unix` if the
> test needs it; otherwise default to `query.since_unix`.

- [ ] **Step 3: Add `futures` to `ohara-core` deps if not already there**

Run: `cargo build -p ohara-core 2>&1 | grep -E "(unresolved import|cannot find)" || echo OK`

Expected: if you see an unresolved-import error for `futures`,
add `futures = { workspace = true }` to
`crates/ohara-core/Cargo.toml` `[dependencies]` (and also add
`futures = "0.3"` to root `Cargo.toml` `[workspace.dependencies]` if
it's not already there). Then re-run `cargo build`.

> Per CONTRIBUTING.md §6, all third-party deps live in workspace
> `[workspace.dependencies]` and crates pull them via `dep.workspace =
> true`. Don't add a per-crate version pin.

- [ ] **Step 4: Run the failing test from Task C.1 and confirm it passes**

Run: `cargo test -p ohara-core --lib disabled_lanes_skip_storage_calls -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run the full retriever test suite**

Run: `cargo test -p ohara-core --lib retriever::`

Expected: every test passes. The phase-event capture test
(`find_pattern_emits_expected_phase_events`) currently asserts the
presence of `lane_knn` / `lane_fts_text` / `lane_fts_sym_hist` /
`lane_fts_sym_head` events; under the hoist these events are emitted
only for enabled lanes. The test runs with the default profile (all
lanes on), so the assertion holds — but verify.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/retriever.rs crates/ohara-core/Cargo.toml Cargo.toml
git commit -m "perf(retriever): hoist lane-mask gates above tokio::join!

Disabled lanes no longer pay for SQL or embed calls; the gates that
were post-hoc filters at retriever.rs:197-215 now wrap each lane
future in OptionFuture so disabled lanes resolve to None without
spawning the underlying work. Headline win is bug_fix /
configuration profiles, which previously paid the full embedding +
4-lane gather cost despite only consuming a subset of the results."
```

---

## Phase D — Final gate

### Task D.1 — Workspace gate + plan status

- [ ] **Step 1: Full workspace gate**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Run: `cargo test --workspace`

Expected: all three pass.

- [ ] **Step 2: Optional — re-run the perf eval to confirm no recall regression**

Run: `cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture`

Expected: `recall_at_5 == 1.0`, `mrr >= 0.80` (matches the published
plan-10 baseline). Plan-24 is a pure latency change; recall metrics
must not move.

- [ ] **Step 3: Update plan status**

Edit
`docs/superpowers/plans/2026-05-05-ohara-plan-24-lane-gate-and-batched-symbols.md`
and change `> **Status:** draft` to `> **Status:** complete`.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/plans/2026-05-05-ohara-plan-24-lane-gate-and-batched-symbols.md
git commit -m "docs(plan-24): mark complete"
```
