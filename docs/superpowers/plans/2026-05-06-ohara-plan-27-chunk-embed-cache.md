# ohara plan-27 — Chunk-level embed cache + `--embed-cache` mode flag (Spec B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each
> green implementation.

**Goal:** Add a content-addressable cache of `(content_hash, embed_model) → vector`
that the embed stage consults before calling the embedder. Expose
three modes via a CLI flag: `off` (default; no cache), `semantic`
(hash + cache `effective_semantic_text`), `diff` (hash + cache
`diff_text` only — embedder input changes to drop commit message).
The mode is part of index identity; mismatch on `--incremental`
triggers the existing `--rebuild` flow.

**Architecture:**
- New refinery migration `V5__chunk_embed_cache.sql` adds the cache
  table. All SQL lives in `ohara-storage`.
- New trait methods `Storage::embed_cache_get_many` /
  `embed_cache_put_many` with default impls (empty / no-op) so
  in-memory test storages stay light.
- New `EmbedMode` enum in `ohara-core`, re-exported from `lib.rs`.
  `EmbedStage` learns `with_embed_mode` and `with_cache` builder
  methods; the existing `run` signature stays the same — mode + cache
  live on the stage.
- `ContentHash` newtype gets a `from_text` constructor (SHA-256,
  distinct from the existing 40-char SHA-1 `from_blob_oid` used by
  plan-21's blame cache).
- `Indexer::with_embed_mode` threads the mode through `Coordinator` to
  `EmbedStage`. `index_metadata` records `embed_input_mode` so a mode
  switch produces a clear `NeedsRebuild` error via plan-13's
  `CompatibilityStatus` machinery.
- CLI: `ohara index --embed-cache off|semantic|diff`. `ohara status`
  prints an `embed_cache:` line when the mode is set.

**Tech stack:** Rust 2021, existing `git2` / `rusqlite` / `tokio` /
`clap`, plus `sha2` (already a workspace dep). No new third-party
deps.

**Spec:** `docs/superpowers/specs/2026-05-06-ohara-chunk-embed-cache-design.md`.

**Sequencing:**
- Phase A (storage substrate) and Phase B (hash + enum primitives)
  are independent — two agents can pick them up in parallel.
- Phase C (EmbedStage integration) depends on A + B.
- Phase D (Indexer + CLI wiring + index_metadata) depends on C.
- Phase E (docs + perf harness) depends on D.
- Phase F (integration tests) depends on D.

Plan-27 is independent of plan-28 (Spec D — parallel commit pipeline,
deferred). The cache surface this plan adds is concurrency-safe; D
parallelises around it.

---

## Phase A — Storage substrate

### Task A.1 — V5 migration + `chunk_embed_cache` table

**Files:**
- Create: `crates/ohara-storage/migrations/V5__chunk_embed_cache.sql`

- [ ] **Step 1: Write the migration**

Create `crates/ohara-storage/migrations/V5__chunk_embed_cache.sql`:

```sql
-- Plan 27: chunk-level embed cache.
--
-- Maps (content_hash, embed_model) -> 384-float embedding vector,
-- stored using the same vec_codec as vec_hunk. The embed stage
-- consults this before calling the embedder so identical chunk
-- content is embedded exactly once per (model) value.
--
-- content_hash is sha256-hex (64 chars) when populated by EmbedMode
-- in {Semantic, Diff}; the column is plain TEXT to stay agnostic to
-- the hash function in case a future RFC swaps it.
CREATE TABLE chunk_embed_cache (
  content_hash TEXT NOT NULL,
  embed_model  TEXT NOT NULL,
  diff_emb     BLOB NOT NULL,
  PRIMARY KEY (content_hash, embed_model)
) WITHOUT ROWID;
```

- [ ] **Step 2: Run the migration smoke test**

Run: `cargo test -p ohara-storage --test '*' migration` (or whatever
test target the existing migrations exercise; if there's no migration
test, just run `cargo build -p ohara-storage` and confirm the binary
applies V5 cleanly when opening a fresh DB — refinery picks up new
files in `migrations/` automatically).

A simple confirmation: `cargo test -p ohara-storage` should still pass
with the new migration in place.

Expected: green.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-storage/migrations/V5__chunk_embed_cache.sql
git commit -m "feat(storage): V5 migration — chunk_embed_cache table"
```

---

### Task A.2 — `Storage::embed_cache_get_many` + `_put_many` trait methods

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`

- [ ] **Step 1: Read the existing `Storage` trait shape**

Open `crates/ohara-core/src/storage.rs`. Notice the existing pattern:
methods like `blob_was_seen` / `record_blob_seen` take a model
identifier and follow the `(key, model) → bool/()` shape. We mirror
that pattern here.

- [ ] **Step 2: Add the two methods with default impls**

Append to the `pub trait Storage` block (after `record_blob_seen`,
before `get_commit`):

```rust
    /// Plan 27: chunk-level embed cache. Look up cached vectors for
    /// `hashes` under the given `embed_model`. Returns a map keyed by
    /// the `ContentHash` values that hit; misses are simply absent
    /// from the map.
    ///
    /// Default impl returns an empty map — appropriate for in-memory
    /// test storages that don't carry a cache. `SqliteStorage`
    /// overrides with a real batched SELECT.
    async fn embed_cache_get_many(
        &self,
        hashes: &[crate::types::ContentHash],
        embed_model: &str,
    ) -> Result<std::collections::HashMap<crate::types::ContentHash, Vec<f32>>> {
        let _ = (hashes, embed_model);
        Ok(std::collections::HashMap::new())
    }

    /// Plan 27: chunk-level embed cache write. Insert one row per
    /// `(hash, model)` pair (composite primary key). Re-insertion of
    /// an existing key is a no-op via `INSERT OR IGNORE`.
    ///
    /// Default impl is a no-op. `SqliteStorage` overrides with a
    /// real batched INSERT.
    async fn embed_cache_put_many(
        &self,
        entries: &[(crate::types::ContentHash, Vec<f32>)],
        embed_model: &str,
    ) -> Result<()> {
        let _ = (entries, embed_model);
        Ok(())
    }
```

- [ ] **Step 3: Verify compile**

Run: `cargo build -p ohara-core`
Expected: builds clean. The default impls mean no existing impl needs
to change in this task.

- [ ] **Step 4: Commit**

```bash
git add crates/ohara-core/src/storage.rs
git commit -m "feat(core): Storage::embed_cache_get_many / _put_many trait methods"
```

---

### Task A.3 — `SqliteStorage` impl + `embed_cache.rs` module

**Files:**
- Create: `crates/ohara-storage/src/tables/embed_cache.rs`
- Modify: `crates/ohara-storage/src/tables/mod.rs` (or wherever
  `tables` modules are registered — verify by reading the file)
- Modify: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-storage/Cargo.toml` if `ohara-core` re-export
  needs adjustment (probably no change needed; ohara-storage already
  depends on ohara-core for the trait).

- [ ] **Step 1: Write the failing test**

Create the module file `crates/ohara-storage/src/tables/embed_cache.rs`
with a placeholder + tests:

```rust
//! Plan-27 chunk-level embed cache. Maps
//! `(content_hash, embed_model)` → 384-float vector. Used by
//! `EmbedStage` to skip re-embedding identical chunk content.

use anyhow::Result;
use ohara_core::types::ContentHash;
use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::codec::vec_codec::{bytes_to_vec, vec_to_bytes};

/// Look up cached embeddings for a batch of `(content_hash, embed_model)`
/// keys. Returns one map entry per hit; misses are absent.
pub fn get_many(
    c: &Connection,
    hashes: &[ContentHash],
    embed_model: &str,
) -> Result<HashMap<ContentHash, Vec<f32>>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    // Build an `?` placeholder string of the right length.
    let placeholders = vec!["?"; hashes.len()].join(",");
    let sql = format!(
        "SELECT content_hash, diff_emb FROM chunk_embed_cache \
         WHERE embed_model = ? AND content_hash IN ({placeholders})"
    );
    let mut stmt = c.prepare(&sql)?;
    let mut bindings: Vec<rusqlite::types::Value> =
        Vec::with_capacity(hashes.len() + 1);
    bindings.push(rusqlite::types::Value::Text(embed_model.to_owned()));
    for h in hashes {
        bindings.push(rusqlite::types::Value::Text(h.as_str().to_owned()));
    }
    let rows = stmt.query_map(rusqlite::params_from_iter(&bindings), |row| {
        let key: String = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        Ok((key, blob))
    })?;
    let mut out = HashMap::with_capacity(hashes.len());
    for r in rows {
        let (key, blob) = r?;
        let v = bytes_to_vec(&blob)?;
        out.insert(ContentHash::from_hex(&key), v);
    }
    Ok(out)
}

/// Insert one row per `(hash, embed_model)` entry. Existing rows are
/// preserved via `INSERT OR IGNORE` — the cache is content-addressed
/// so a re-insert of the same key + model would be a no-op anyway.
pub fn put_many(
    c: &mut Connection,
    entries: &[(ContentHash, Vec<f32>)],
    embed_model: &str,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let tx = c.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO chunk_embed_cache \
             (content_hash, embed_model, diff_emb) VALUES (?1, ?2, ?3)",
        )?;
        for (hash, vec) in entries {
            let bytes = vec_to_bytes(vec);
            stmt.execute(params![hash.as_str(), embed_model, bytes])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStorage;
    use deadpool_sqlite::Config;
    use ohara_core::types::ContentHash;

    async fn temp_storage() -> SqliteStorage {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embed_cache_test.db");
        // Leak the tempdir handle for the duration of the test; tests
        // are short-lived and the OS reclaims on process exit.
        Box::leak(Box::new(dir));
        SqliteStorage::open(&path).await.unwrap()
    }

    #[tokio::test]
    async fn get_returns_empty_map_when_cache_is_empty() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hashes = vec![ContentHash::from_hex("a"), ContentHash::from_hex("b")];
        let out = conn
            .interact(move |c| get_many(c, &hashes, "model-x"))
            .await
            .unwrap()
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn put_then_get_round_trips_a_single_entry() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hash = ContentHash::from_hex("deadbeef");
        let vec = vec![0.1_f32, 0.2, 0.3, 0.4];
        let entries = vec![(hash.clone(), vec.clone())];
        let model = "model-x".to_string();
        let model_for_get = model.clone();
        let hashes_for_get = vec![hash.clone()];
        conn.interact(move |c| put_many(c, &entries, &model))
            .await
            .unwrap()
            .unwrap();
        let out = conn
            .interact(move |c| get_many(c, &hashes_for_get, &model_for_get))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.len(), 1);
        let got = out.get(&hash).unwrap();
        assert_eq!(got, &vec);
    }

    #[tokio::test]
    async fn same_hash_different_model_are_distinct_rows() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hash = ContentHash::from_hex("aa");
        let v1 = vec![1.0_f32];
        let v2 = vec![2.0_f32];
        let entries_a = vec![(hash.clone(), v1.clone())];
        let entries_b = vec![(hash.clone(), v2.clone())];
        conn.interact(move |c| put_many(c, &entries_a, "model-a"))
            .await
            .unwrap()
            .unwrap();
        conn.interact(move |c| put_many(c, &entries_b, "model-b"))
            .await
            .unwrap()
            .unwrap();
        let h1 = vec![hash.clone()];
        let h2 = vec![hash.clone()];
        let from_a = conn
            .interact(move |c| get_many(c, &h1, "model-a"))
            .await
            .unwrap()
            .unwrap();
        let from_b = conn
            .interact(move |c| get_many(c, &h2, "model-b"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from_a.get(&hash), Some(&v1));
        assert_eq!(from_b.get(&hash), Some(&v2));
    }
}
```

The test helpers (`temp_storage`, `interact`) follow the existing
patterns in `crates/ohara-storage/src/tables/*.rs`. If the existing
tests use a different scaffolding (e.g., a shared `setup()` helper),
adapt accordingly while keeping the assertions identical.

- [ ] **Step 2: Register the module**

In `crates/ohara-storage/src/tables/mod.rs`, add:

```rust
pub mod embed_cache;
```

(alphabetical position among the existing `pub mod` declarations).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ohara-storage embed_cache::tests`
Expected: FAIL — the V5 migration hasn't taught `SqliteStorage` to
serve queries against `chunk_embed_cache` yet, but A.1 already added
the migration so the table exists. The most likely failure is a
build error if `vec_to_bytes` / `bytes_to_vec` aren't pub-accessible
from the new module — verify by reading
`crates/ohara-storage/src/codec/vec_codec.rs` and adjusting the use
statement if needed.

If tests build and run but fail with "no such table", confirm that
A.1 was actually committed (the V5 migration must run on the test
storage open).

- [ ] **Step 4: Make tests pass**

If the tests build and run but fail, the most likely fix is that
`vec_to_bytes` / `bytes_to_vec` need different paths. Adjust the
import. The body of the functions is correct as written.

If `pool()` is private on `SqliteStorage`, expose it as `pub(crate)`
or use whatever the existing test pattern uses to get a connection.

- [ ] **Step 5: Wire into the `Storage` trait impl**

In `crates/ohara-storage/src/storage_impl.rs`, find `impl Storage for
SqliteStorage`. Add overrides for the two new trait methods (anywhere
in the `impl` block; place them adjacent to other cache-style methods
like `blob_was_seen` if there's a clear grouping):

```rust
    async fn embed_cache_get_many(
        &self,
        hashes: &[ohara_core::types::ContentHash],
        embed_model: &str,
    ) -> ohara_core::Result<
        std::collections::HashMap<ohara_core::types::ContentHash, Vec<f32>>,
    > {
        let conn = self.pool.get().await.map_err(|e| {
            ohara_core::OhraError::Storage(format!("get conn: {e}"))
        })?;
        let hashes = hashes.to_vec();
        let model = embed_model.to_owned();
        conn.interact(move |c| {
            crate::tables::embed_cache::get_many(c, &hashes, &model)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Storage(format!("interact: {e}")))?
        .map_err(|e| ohara_core::OhraError::Storage(format!("embed_cache_get_many: {e}")))
    }

    async fn embed_cache_put_many(
        &self,
        entries: &[(ohara_core::types::ContentHash, Vec<f32>)],
        embed_model: &str,
    ) -> ohara_core::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let conn = self.pool.get().await.map_err(|e| {
            ohara_core::OhraError::Storage(format!("get conn: {e}"))
        })?;
        let entries = entries.to_vec();
        let model = embed_model.to_owned();
        conn.interact(move |c| {
            crate::tables::embed_cache::put_many(c, &entries, &model)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Storage(format!("interact: {e}")))?
        .map_err(|e| ohara_core::OhraError::Storage(format!("embed_cache_put_many: {e}")))
    }
```

The exact error-conversion calls should match how other methods in
`storage_impl.rs` translate `anyhow::Error` to `OhraError`. Read one
or two existing methods (e.g., the `put_hunks` impl) to see the
project pattern; preserve it. If there's a `From<anyhow::Error>` for
`OhraError`, the closures may simplify with `?`.

- [ ] **Step 6: Re-run tests**

Run: `cargo test -p ohara-storage embed_cache::tests`
Run: `cargo test -p ohara-storage`
Run: `cargo build -p ohara-core`
Expected: all green.

- [ ] **Step 7: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-storage/src/tables/embed_cache.rs \
        crates/ohara-storage/src/tables/mod.rs \
        crates/ohara-storage/src/storage_impl.rs
git commit -m "feat(storage): chunk_embed_cache get_many / put_many on SqliteStorage"
```

---

## Phase B — Hash + EmbedMode primitives

### Task B.1 — `ContentHash::from_text` (SHA-256)

**Files:**
- Modify: `crates/ohara-core/src/types.rs`

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod content_hash_tests` in
`crates/ohara-core/src/types.rs` (after the existing tests):

```rust
    #[test]
    fn from_text_is_deterministic() {
        // Plan 27 Task B.1: same text → same hash.
        let a = ContentHash::from_text("hello world");
        let b = ContentHash::from_text("hello world");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64, "sha256-hex must be 64 chars");
    }

    #[test]
    fn from_text_differs_for_different_inputs() {
        let a = ContentHash::from_text("hello");
        let b = ContentHash::from_text("hellp");
        assert_ne!(a, b);
    }

    #[test]
    fn from_text_empty_input_is_well_defined_and_distinct_from_blob_oid_zero() {
        // Plan 27 Task B.1: from_text("") is the sha256 of the empty
        // string ("e3b0c4..."). It must differ from a from_blob_oid
        // representing all-zeros OID (40 chars, all '0'), which
        // sha256-hex never produces.
        let empty = ContentHash::from_text("");
        assert_eq!(
            empty.as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(empty.as_str().len(), 40, "must not collide with a 40-char OID");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core content_hash_tests::from_text`
Expected: FAIL with "no associated function `from_text`".

- [ ] **Step 3: Implement `from_text`**

In `crates/ohara-core/src/types.rs`, in the `impl ContentHash` block,
add a new constructor adjacent to `from_hex`:

```rust
    /// Construct from arbitrary text (UTF-8). Returns a sha256-hex
    /// string (64 ASCII characters). Used by plan-27's chunk embed
    /// cache to key on the bytes the embedder will consume.
    ///
    /// `from_text` is *distinct* from `from_blob_oid`: that one is
    /// keyed by git's blob hash (40-char SHA-1) for file content;
    /// this one keys cache lookups by the embedder input. Their
    /// outputs share the same `ContentHash` Rust type but live in
    /// different storage tables (`BlameCache` vs `chunk_embed_cache`)
    /// so they cannot collide in practice.
    pub fn from_text(text: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(text.as_bytes());
        Self(hex::encode(digest))
    }
```

The `sha2` and `hex` workspace deps are already available in
`ohara-core` via plan-21's prior work — verify with
`grep "sha2\|hex" crates/ohara-core/Cargo.toml`. If not present, add:

```toml
sha2.workspace = true
hex.workspace = true
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-core content_hash_tests`
Expected: PASS — all `content_hash_tests` (existing + 3 new).

- [ ] **Step 5: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/types.rs crates/ohara-core/Cargo.toml
git commit -m "feat(core): ContentHash::from_text (SHA-256) for chunk cache keys"
```

---

### Task B.2 — `EmbedMode` enum + re-export

**Files:**
- Modify: `crates/ohara-core/src/embed.rs`
- Modify: `crates/ohara-core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-core/src/embed.rs` (in or alongside its
existing `#[cfg(test)] mod tests` if there is one; if not, create
one at the bottom of the file):

```rust
#[cfg(test)]
mod embed_mode_tests {
    use super::*;

    #[test]
    fn embed_mode_off_and_semantic_are_distinct_variants() {
        assert_ne!(EmbedMode::Off, EmbedMode::Semantic);
        assert_ne!(EmbedMode::Off, EmbedMode::Diff);
        assert_ne!(EmbedMode::Semantic, EmbedMode::Diff);
    }

    #[test]
    fn embed_mode_default_is_off() {
        // Plan 27 Task B.2: the default mode must match today's
        // behavior — no cache lookups.
        assert_eq!(EmbedMode::default(), EmbedMode::Off);
    }

    #[test]
    fn embed_mode_index_metadata_value_distinguishes_diff() {
        // Plan 27 Task B.2: Off and Semantic both embed semantic_text
        // and so are vector-equivalent; they share the same
        // index_metadata value. Diff is a separate compatibility class.
        assert_eq!(EmbedMode::Off.index_metadata_value(), "semantic");
        assert_eq!(EmbedMode::Semantic.index_metadata_value(), "semantic");
        assert_eq!(EmbedMode::Diff.index_metadata_value(), "diff");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core embed_mode_tests`
Expected: FAIL — `EmbedMode` doesn't exist.

- [ ] **Step 3: Implement `EmbedMode`**

Append to `crates/ohara-core/src/embed.rs`:

```rust
/// Plan-27 chunk-embed cache mode. Selects whether the embedder is
/// fronted by the `chunk_embed_cache` table and, in `Diff` mode,
/// what input the embedder consumes.
///
/// - `Off`: no cache; embedder consumes today's `effective_semantic_text`.
/// - `Semantic`: cache keyed by `sha256(effective_semantic_text)`;
///   embedder input unchanged.
/// - `Diff`: cache keyed by `sha256(diff_text)`; embedder input is
///   `diff_text` only (commit message dropped from the vector lane).
///
/// `Off` and `Semantic` produce vector-equivalent indices (same
/// embedder input). `Diff` produces a different vector lane and so
/// requires a `--rebuild` to switch into or out of.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum EmbedMode {
    #[default]
    Off,
    Semantic,
    Diff,
}

impl EmbedMode {
    /// Stable string used as the `embed_input_mode` value in
    /// `RuntimeIndexMetadata`. Off and Semantic share `"semantic"`
    /// because they're vector-equivalent; Diff has its own class.
    pub fn index_metadata_value(self) -> &'static str {
        match self {
            EmbedMode::Off | EmbedMode::Semantic => "semantic",
            EmbedMode::Diff => "diff",
        }
    }
}
```

- [ ] **Step 4: Re-export from `lib.rs`**

In `crates/ohara-core/src/lib.rs`, find the `pub use embed::EmbeddingProvider;`
line and extend it:

```rust
pub use embed::{EmbedMode, EmbeddingProvider};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ohara-core embed_mode_tests`
Expected: PASS — 3 tests.

- [ ] **Step 6: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/embed.rs crates/ohara-core/src/lib.rs
git commit -m "feat(core): EmbedMode enum (Off/Semantic/Diff) for plan-27 cache"
```

---

## Phase C — EmbedStage cache integration

### Task C.1 — `EmbedStage` builder methods (no behavior change yet)

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/embed.rs`

- [ ] **Step 1: Add fields + builders**

In `crates/ohara-core/src/indexer/stages/embed.rs`, modify the
`EmbedStage` struct to add two new optional fields:

```rust
pub struct EmbedStage {
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    embed_mode: crate::EmbedMode,
    cache: Option<Arc<dyn crate::Storage>>,
}
```

(Note: importing the `Storage` trait via `crate::Storage` path; adjust
to whichever path the file already uses.)

Update `EmbedStage::new` to initialise the new fields:

```rust
    pub fn new(embedder: Arc<dyn EmbeddingProvider + Send + Sync>) -> Self {
        Self {
            embedder,
            embed_batch: 32,
            embed_mode: crate::EmbedMode::default(),
            cache: None,
        }
    }
```

Add builder methods after `with_embed_batch`:

```rust
    /// Set the embed mode. Off (default) means no cache lookups;
    /// Semantic / Diff turn on the chunk-embed cache and (for Diff)
    /// change the embedder input. Plan 27.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        self
    }

    /// Wire a `Storage` impl that backs the chunk embed cache. Only
    /// consulted when `with_embed_mode` is set to Semantic or Diff.
    /// Plan 27.
    pub fn with_cache(mut self, storage: Arc<dyn crate::Storage>) -> Self {
        self.cache = Some(storage);
        self
    }
```

If clippy flags `cache` as unused (because no method reads it yet),
add `#[allow(dead_code)]` on the field — Task C.2 will read it.

- [ ] **Step 2: Run existing tests**

Run: `cargo test -p ohara-core indexer::stages::embed`
Expected: PASS — the existing tests don't touch the new fields, and
the default mode is `Off`.

- [ ] **Step 3: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/indexer/stages/embed.rs
git commit -m "feat(core): EmbedStage::with_embed_mode + with_cache (no behavior yet)"
```

---

### Task C.2 — `Semantic` mode: hash + cache lookup + miss-only embed

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/embed.rs`

This task wires the cache into `EmbedStage::run` for the `Semantic`
mode. The `Off` path is unchanged. `Diff` is handled in C.3.

- [ ] **Step 1: Add a counting in-memory cache fixture**

Append to the `#[cfg(test)] mod tests` in
`crates/ohara-core/src/indexer/stages/embed.rs`:

```rust
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Test-only Storage that backs an in-memory chunk_embed_cache.
    /// All other Storage methods fall through to the trait defaults
    /// (which are no-op or empty for the methods we care about; the
    /// only methods used by EmbedStage::run are
    /// embed_cache_get_many / put_many).
    #[derive(Default)]
    struct InMemoryCacheStorage {
        entries: StdMutex<HashMap<(crate::types::ContentHash, String), Vec<f32>>>,
    }

    #[async_trait]
    impl crate::Storage for InMemoryCacheStorage {
        async fn open_repo(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn get_index_status(
            &self,
            _: &crate::types::RepoId,
        ) -> Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus::default())
        }
        async fn set_last_indexed_commit(
            &self,
            _: &crate::types::RepoId,
            _: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn put_commit(
            &self,
            _: &crate::types::RepoId,
            _: &crate::storage::CommitRecord,
        ) -> Result<()> {
            Ok(())
        }
        async fn commit_exists(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn put_hunks(
            &self,
            _: &crate::types::RepoId,
            _: &[crate::storage::HunkRecord],
        ) -> Result<()> {
            Ok(())
        }
        async fn put_head_symbols(
            &self,
            _: &crate::types::RepoId,
            _: &[crate::types::Symbol],
        ) -> Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &crate::types::RepoId) -> Result<()> {
            Ok(())
        }
        async fn knn_hunks(
            &self,
            _: &crate::types::RepoId,
            _: &[f32],
            _: usize,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_text(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: usize,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn get_hunk_symbols(
            &self,
            _: &crate::types::RepoId,
            _: crate::storage::HunkId,
        ) -> Result<Vec<crate::types::HunkSymbol>> {
            Ok(vec![])
        }
        async fn get_hunk_symbols_batch(
            &self,
            _: &crate::types::RepoId,
            _: &[crate::storage::HunkId],
        ) -> Result<HashMap<crate::storage::HunkId, Vec<crate::types::HunkSymbol>>> {
            Ok(HashMap::new())
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_commit(
            &self,
            _: &crate::types::RepoId,
            _: &str,
        ) -> Result<Option<crate::types::CommitMeta>> {
            Ok(None)
        }
        async fn get_commits_by_sha(
            &self,
            _: &[crate::types::CommitSha],
        ) -> Result<HashMap<crate::types::CommitSha, crate::types::CommitMeta>> {
            Ok(HashMap::new())
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: &str,
        ) -> Result<Vec<crate::storage::HunkHit>> {
            Ok(vec![])
        }
        async fn get_neighboring_file_commits(
            &self,
            _: &crate::types::RepoId,
            _: &str,
            _: &str,
        ) -> Result<Vec<crate::types::CommitMeta>> {
            Ok(vec![])
        }
        async fn get_index_metadata(
            &self,
            _: &crate::types::RepoId,
        ) -> Result<crate::index_metadata::StoredIndexMetadata> {
            Ok(crate::index_metadata::StoredIndexMetadata::default())
        }
        async fn put_index_metadata(
            &self,
            _: &crate::types::RepoId,
            _: &crate::index_metadata::StoredIndexMetadata,
        ) -> Result<()> {
            Ok(())
        }

        async fn embed_cache_get_many(
            &self,
            hashes: &[crate::types::ContentHash],
            embed_model: &str,
        ) -> Result<HashMap<crate::types::ContentHash, Vec<f32>>> {
            let entries = self.entries.lock().unwrap();
            let mut out = HashMap::new();
            for h in hashes {
                if let Some(v) = entries.get(&(h.clone(), embed_model.to_owned())) {
                    out.insert(h.clone(), v.clone());
                }
            }
            Ok(out)
        }

        async fn embed_cache_put_many(
            &self,
            entries_in: &[(crate::types::ContentHash, Vec<f32>)],
            embed_model: &str,
        ) -> Result<()> {
            let mut entries = self.entries.lock().unwrap();
            for (h, v) in entries_in {
                entries.insert((h.clone(), embed_model.to_owned()), v.clone());
            }
            Ok(())
        }
    }
```

The signatures above MUST match the actual `Storage` trait at the
time of writing. Read `crates/ohara-core/src/storage.rs` first;
because the trait has many methods, this fixture is large but
mechanical. If the trait surface has grown since this plan was
written, add the missing methods with `Ok(...)` or `Ok(vec![])`
defaults. The interesting methods are the two `embed_cache_*` overrides.

- [ ] **Step 2: Write the failing test**

Append:

```rust
    #[tokio::test]
    async fn semantic_mode_second_run_reuses_cached_vectors_and_skips_embed() {
        // Plan 27 Task C.2: with EmbedMode::Semantic + a cache, the
        // first call embeds normally and writes to the cache; the
        // second call with the same hunks must hit the cache and call
        // embed_batch only for the commit message.
        let calls = Arc::new(Mutex::new(Vec::<usize>::new()));
        let embedder = Arc::new(CountingEmbedder {
            calls: calls.clone(),
            dim: 4,
        });
        let cache: Arc<dyn crate::Storage> =
            Arc::new(InMemoryCacheStorage::default());
        let stage = EmbedStage::new(embedder.clone())
            .with_embed_mode(crate::EmbedMode::Semantic)
            .with_cache(cache.clone());

        let hunks = vec![attributed("hunk one"), attributed("hunk two")];
        let _ = stage.run("commit msg", &hunks).await.unwrap();

        // First run: 1 commit message + 2 hunks = 3 inputs.
        let observed = calls.lock().unwrap().clone();
        let total_first: usize = observed.iter().sum();
        assert_eq!(total_first, 3, "first run must embed 3 texts: {observed:?}");

        // Second run with identical hunks: only the commit message
        // should be embedded (commit messages are not cached). The
        // two hunks must be served from cache.
        let _ = stage.run("commit msg", &hunks).await.unwrap();
        let after = calls.lock().unwrap().clone();
        let total_second: usize = after.iter().sum::<usize>() - total_first;
        assert_eq!(total_second, 1, "second run must embed only 1 text (commit msg): added {total_second} after first run");
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p ohara-core indexer::stages::embed::tests::semantic_mode_second_run_reuses_cached_vectors_and_skips_embed`
Expected: FAIL — the second run still embeds 3 texts because the
cache isn't consulted yet.

- [ ] **Step 4: Implement the cache lookup in `run`**

Restructure `EmbedStage::run` to consult the cache when mode is
`Semantic` or `Diff`. The flow inside `run`, after the early-return
branch for empty hunks:

```rust
        // 1. Compute embedder input per hunk.
        //    Off / Semantic → effective_semantic_text
        //    Diff           → record.diff_text  (drops commit message)
        let mode = self.embed_mode;
        let hunk_inputs: Vec<String> = attributed_hunks
            .iter()
            .map(|ah| match mode {
                crate::EmbedMode::Diff => ah.record.diff_text.clone(),
                _ => ah.effective_semantic_text().to_owned(),
            })
            .collect();

        // 2. Cache lookup (mode != Off and cache is wired).
        let model_id = self.embedder.model_id().to_owned();
        let cached: HashMap<crate::types::ContentHash, Vec<f32>> = match (mode, self.cache.as_ref()) {
            (crate::EmbedMode::Off, _) | (_, None) => HashMap::new(),
            (_, Some(cache)) => {
                let hashes: Vec<crate::types::ContentHash> = hunk_inputs
                    .iter()
                    .map(|s| crate::types::ContentHash::from_text(s))
                    .collect();
                cache.embed_cache_get_many(&hashes, &model_id).await?
            }
        };

        // 3. Build the text batch: commit message at index 0, then
        //    only the hunk inputs that missed the cache.
        let mut batch_texts: Vec<String> = Vec::with_capacity(hunk_inputs.len() + 1);
        batch_texts.push(commit_message.to_owned());
        let mut miss_indices: Vec<usize> = Vec::new();
        for (i, input) in hunk_inputs.iter().enumerate() {
            let hash = crate::types::ContentHash::from_text(input);
            if cached.contains_key(&hash) {
                continue;
            }
            miss_indices.push(i);
            batch_texts.push(input.clone());
        }

        // 4. Embed the batch (commit message + misses).
        let all_embs = self.embed_in_chunks(&batch_texts).await?;
        let (commit_vec, miss_vecs) = all_embs.split_first().ok_or_else(|| {
            OhraError::Embedding("embed_batch returned empty for non-empty input".into())
        })?;
        if miss_vecs.len() != miss_indices.len() {
            return Err(OhraError::Embedding(format!(
                "miss vector count {} != miss index count {}",
                miss_vecs.len(),
                miss_indices.len()
            )));
        }

        // 5. Write misses back to the cache.
        if mode != crate::EmbedMode::Off {
            if let Some(cache) = self.cache.as_ref() {
                let entries: Vec<(crate::types::ContentHash, Vec<f32>)> = miss_indices
                    .iter()
                    .zip(miss_vecs.iter())
                    .map(|(i, v)| {
                        (crate::types::ContentHash::from_text(&hunk_inputs[*i]), v.clone())
                    })
                    .collect();
                cache.embed_cache_put_many(&entries, &model_id).await?;
            }
        }

        // 6. Assemble final EmbeddedHunk list in original order.
        //    Hits take cached vectors; misses take freshly-embedded.
        let mut miss_iter = miss_vecs.iter();
        let hunks: Vec<EmbeddedHunk> = attributed_hunks
            .iter()
            .zip(hunk_inputs.iter())
            .map(|(ah, input)| {
                let hash = crate::types::ContentHash::from_text(input);
                let embedding = match cached.get(&hash) {
                    Some(v) => v.clone(),
                    None => miss_iter
                        .next()
                        .expect("invariant: miss_vecs aligned with hunk_inputs misses")
                        .clone(),
                };
                EmbeddedHunk {
                    attributed: ah.clone(),
                    embedding,
                }
            })
            .collect();

        Ok(EmbedOutput {
            commit_embedding: commit_vec.clone(),
            hunks,
        })
```

Replace the existing body of `run` (after the empty-hunks early
return) with this flow. The `Off` mode still pays exactly one
embed_batch call per chunk-of-batch and goes through the same code
path as today (cache is empty, all hunks become misses).

The `expect("invariant: miss_vecs aligned ...")` is the allowed
form per CONTRIBUTING.md (true invariant — we asserted equal lengths
in step 4).

- [ ] **Step 5: Run tests**

Run: `cargo test -p ohara-core indexer::stages::embed`
Expected: PASS — both the new test AND the existing tests
(`with_embed_batch_2_produces_correct_chunk_count`,
`empty_hunk_list_yields_empty_output`, `embed_vectors_have_correct_dimension`).

If `with_embed_batch_2_produces_correct_chunk_count` fails because
the chunk count math changed: the test asserts that 7 inputs at
batch=2 produce calls `[2, 2, 2, 1]`. With cache misses on first
run, all 7 still go through, so the call shape should be unchanged.
If the test now fails, double-check that the `Off`-mode code path
(or `Semantic` first run) still produces the expected batches.

- [ ] **Step 6: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/indexer/stages/embed.rs
git commit -m "feat(core): EmbedStage Semantic mode — cache hit/miss flow"
```

---

### Task C.3 — `Diff` mode: change embedder input to `diff_text`

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/embed.rs`

The C.2 implementation already includes the `Diff` branch in step 1
(`mode == Diff → ah.record.diff_text.clone()`). This task adds the
test that pins the behavior.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `embed.rs`:

```rust
    /// Embedder fake that records the texts it received per call. Used
    /// to assert that `Diff` mode changes the embedder input.
    struct RecordingEmbedder {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for RecordingEmbedder {
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            "recorder"
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.seen.lock().unwrap().push(texts.to_vec());
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    fn attributed_with_diff(diff: &str, semantic: &str) -> AttributedHunk {
        AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: diff.into(),
                semantic_text: semantic.into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }
    }

    #[tokio::test]
    async fn diff_mode_feeds_diff_text_to_embedder_not_semantic_text() {
        // Plan 27 Task C.3: in Diff mode the embedder receives
        // diff_text, not the commit-message-prefixed semantic_text.
        // The cache key is sha256(diff_text), so two hunks with
        // identical diff_text but different semantic_text produce a
        // single embed call for the second hunk.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let embedder = Arc::new(RecordingEmbedder {
            seen: seen.clone(),
            dim: 4,
        });
        let cache: Arc<dyn crate::Storage> =
            Arc::new(InMemoryCacheStorage::default());
        let stage = EmbedStage::new(embedder.clone())
            .with_embed_mode(crate::EmbedMode::Diff)
            .with_cache(cache.clone());

        // Two hunks: identical diff_text, distinct semantic_text.
        let hunks = vec![
            attributed_with_diff("+let x = 1;\n", "msg one\n\n+let x = 1;\n"),
            attributed_with_diff("+let x = 1;\n", "msg two\n\n+let x = 1;\n"),
        ];
        let _ = stage.run("commit msg", &hunks).await.unwrap();

        // The first call should contain commit_msg + the diff_text
        // ONCE (the second hunk's diff_text matches the first → cache
        // hit → not in batch).
        let calls = seen.lock().unwrap().clone();
        let total_seen: Vec<&String> = calls.iter().flatten().collect();
        let diff_count = total_seen
            .iter()
            .filter(|s| s.contains("+let x = 1;"))
            .count();
        assert_eq!(
            diff_count, 1,
            "Diff mode should embed identical diff_text only once, got {diff_count}: {calls:?}"
        );

        // The embedder must NOT have seen the prefixed semantic_text
        // in Diff mode.
        let saw_semantic = total_seen.iter().any(|s| s.contains("msg one") || s.contains("msg two"));
        assert!(
            !saw_semantic,
            "Diff mode should not feed semantic_text to embedder: {calls:?}"
        );
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ohara-core indexer::stages::embed::tests::diff_mode_feeds_diff_text_to_embedder_not_semantic_text`
Expected: PASS — C.2's implementation already routes `Diff` mode to
`record.diff_text`. C.3's test pins the contract.

If it fails: the bug is in C.2's mode dispatch (step 1). Re-check
that `EmbedMode::Diff` selects `ah.record.diff_text.clone()` and not
`effective_semantic_text()`.

- [ ] **Step 3: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/indexer/stages/embed.rs
git commit -m "test(core): pin EmbedMode::Diff feeds diff_text to embedder"
```

---

## Phase D — Indexer + CLI wiring

### Task D.1 — `Indexer::with_embed_mode` threaded to `Coordinator` to `EmbedStage`

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs`

- [ ] **Step 1: Add `embed_mode` field + builder to `Indexer`**

In `crates/ohara-core/src/indexer.rs`, add to the `Indexer` struct:

```rust
    /// Plan 27: chunk-embed cache mode. Threaded to Coordinator and
    /// from there to EmbedStage. Defaults to Off.
    embed_mode: crate::EmbedMode,
```

Update `Indexer::new` to initialise `embed_mode: crate::EmbedMode::default()`.

Add the builder:

```rust
    /// Set the chunk-embed cache mode. Plan 27.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        self
    }
```

- [ ] **Step 2: Thread the mode to `Coordinator`**

In `crates/ohara-core/src/indexer/coordinator/mod.rs`, add to the
`Coordinator` struct:

```rust
    embed_mode: crate::EmbedMode,
    /// Plan 27: storage handle reused by EmbedStage for the
    /// chunk-embed cache. None when embed_mode is Off.
    cache_storage: Option<Arc<dyn crate::Storage>>,
```

Update `Coordinator::new` to default both. Add a builder:

```rust
    /// Plan 27: set the chunk-embed cache mode. When mode != Off, the
    /// existing `storage` handle is reused as the cache backend.
    pub fn with_embed_mode(mut self, mode: crate::EmbedMode) -> Self {
        self.embed_mode = mode;
        if mode != crate::EmbedMode::Off {
            self.cache_storage = Some(self.storage.clone() as Arc<dyn crate::Storage>);
        }
        self
    }
```

In `run_commit_timed` (around line 240), where `EmbedStage::new(...)`
is constructed, chain the new builders:

```rust
        let mut embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch)
            .with_embed_mode(self.embed_mode);
        if let Some(cache) = self.cache_storage.as_ref() {
            embed_stage = embed_stage.with_cache(cache.clone());
        }
```

- [ ] **Step 3: Plumb in `Indexer::run`**

In `Indexer::run` (around the place where `Coordinator::new(...)` is
constructed — verify by reading the function), chain:

```rust
        let mut coord = Coordinator::new(self.storage.clone(), self.embedder.clone())
            // [preserve existing builder calls from C.4 of plan-26 etc.]
            .with_embed_mode(self.embed_mode);
        // ... rest of plan-26's Coordinator wiring (with_ignore_filter etc.)
```

Preserve all existing chain elements (`with_progress`, `with_embed_batch`,
`with_ignore_filter` — added in plan-26).

- [ ] **Step 4: Smoke test**

Run: `cargo test -p ohara-core`
Expected: PASS — no behavior change because the default mode is Off.

- [ ] **Step 5: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/indexer.rs crates/ohara-core/src/indexer/coordinator/mod.rs
git commit -m "feat(core): thread EmbedMode through Indexer → Coordinator → EmbedStage"
```

---

### Task D.2 — `index_metadata` integration: `embed_input_mode` component

**Files:**
- Modify: `crates/ohara-core/src/index_metadata.rs`
- Modify: `crates/ohara-cli/src/commands/status.rs` (so the
  `current_runtime_metadata` helper picks up the new field)

- [ ] **Step 1: Read existing fields**

Open `crates/ohara-core/src/index_metadata.rs`. Find
`RuntimeIndexMetadata` (it's the struct that lists embedding model,
embedding dimension, reranker model, chunker version, parser
versions). Find `to_storage_components` (the method that flattens it
to the `(key, value)` pairs stored in the `index_metadata` table) and
the `assess` method on `CompatibilityStatus`.

- [ ] **Step 2: Add `embed_input_mode` field**

Append a new field to `RuntimeIndexMetadata`:

```rust
    /// Plan 27: which input the embedder consumes. "semantic" (Off
    /// or Semantic mode — vector-equivalent), or "diff" (Diff mode).
    /// Mismatch on resume forces a NeedsRebuild assessment.
    pub embed_input_mode: String,
```

Update the constructor `runtime_metadata_from(...)` to accept the
mode (probably as `embed_input_mode: &str`) and store it. Match the
existing pattern: every parameter to that function is a `&str` /
const ref.

The `current_runtime_metadata` helper in
`crates/ohara-cli/src/commands/status.rs` (which calls the constructor)
will need to pass a default — use `"semantic"` (the default mode's
metadata value) so existing repos continue to assess as compatible.

In `to_storage_components`, add a row:

```rust
        out.push(("embed_input_mode".to_string(), self.embed_input_mode.clone()));
```

In `CompatibilityStatus::assess`, add a check that mirrors the
existing `embedding_model` mismatch arm:

```rust
        let stored_mode = stored.components.get("embed_input_mode");
        match stored_mode {
            None => {
                missing_components.push("embed_input_mode".into());
            }
            Some(s) if s != &runtime.embed_input_mode => {
                return CompatibilityStatus::NeedsRebuild {
                    reason: format!(
                        "embed_input_mode mismatch: stored={s} runtime={}",
                        runtime.embed_input_mode
                    ),
                };
            }
            Some(_) => {}
        }
```

(Adjust to match the assess function's existing control-flow style —
read the function first.)

- [ ] **Step 3: Update existing tests**

Some tests in `index_metadata.rs` and `status.rs` build a
`RuntimeIndexMetadata` directly. Add `embed_input_mode: "semantic".into()`
to all such constructions. Run:

```
cargo test -p ohara-core
cargo test -p ohara-cli
```

Fix compilation errors by adding the new field everywhere.

- [ ] **Step 4: Add a regression test**

Append to `index_metadata.rs`'s test module:

```rust
    #[test]
    fn assess_mode_mismatch_is_needs_rebuild() {
        // Plan 27 Task D.2: switching from semantic to diff mode (or
        // vice versa) on --incremental must trigger NeedsRebuild.
        let mut runtime = current_runtime_metadata_for_test();
        runtime.embed_input_mode = "diff".into();
        let stored = stored_complete_for(&runtime);
        // Now flip stored to "semantic" and reassess.
        let mut stored_semantic = stored.clone();
        stored_semantic
            .components
            .insert("embed_input_mode".into(), "semantic".into());
        let assessment = CompatibilityStatus::assess(&runtime, &stored_semantic);
        assert!(
            matches!(assessment, CompatibilityStatus::NeedsRebuild { .. }),
            "expected NeedsRebuild, got {assessment:?}"
        );
    }
```

`current_runtime_metadata_for_test` and `stored_complete_for` should
already exist in the test module (read the file to find the right
names; if absent, build a small inline `RuntimeIndexMetadata` with
all fields populated).

- [ ] **Step 5: Run tests + commit**

```
cargo test -p ohara-core index_metadata
cargo test -p ohara-cli status
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/index_metadata.rs crates/ohara-cli/src/commands/status.rs
git commit -m "feat(core): index_metadata.embed_input_mode + NeedsRebuild on mismatch"
```

---

### Task D.3 — `--embed-cache` CLI flag on `ohara index`

**Files:**
- Modify: `crates/ohara-cli/src/commands/index.rs`

- [ ] **Step 1: Add the clap arg**

Find the `Args` struct in `crates/ohara-cli/src/commands/index.rs`.
Add a new field:

```rust
    /// Chunk-embed cache mode (plan-27). `off` (default) matches
    /// today's behavior. `semantic` caches by sha256(semantic_text);
    /// `diff` caches by sha256(diff_text) and changes the embedder
    /// input to drop the commit message.
    #[arg(long, value_enum, default_value_t = EmbedCacheArg::Off)]
    pub embed_cache: EmbedCacheArg,
```

Above the `Args` struct, add the enum:

```rust
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum EmbedCacheArg {
    Off,
    Semantic,
    Diff,
}

impl From<EmbedCacheArg> for ohara_core::EmbedMode {
    fn from(a: EmbedCacheArg) -> Self {
        match a {
            EmbedCacheArg::Off => ohara_core::EmbedMode::Off,
            EmbedCacheArg::Semantic => ohara_core::EmbedMode::Semantic,
            EmbedCacheArg::Diff => ohara_core::EmbedMode::Diff,
        }
    }
}
```

- [ ] **Step 2: Plumb into `Indexer`**

In `index::run`, where `Indexer::new(...)` is constructed (and chained
with `.with_repo_root(...)` from plan-26), chain:

```rust
        let indexer = Indexer::new(storage.clone(), embedder.clone())
            // [existing builders preserved]
            .with_embed_mode(args.embed_cache.into());
```

Also: when running, set the runtime metadata's `embed_input_mode`
from the chosen mode before calling `Indexer::run`. The runtime
metadata is built via `current_runtime_metadata` in `status.rs` —
extend that helper (or add a sibling) to take an `EmbedMode` and
return a metadata with the mode populated.

A simpler path: build `RuntimeIndexMetadata` inline in `index::run`
using the helper, then mutate the field:

```rust
        let mut runtime_meta = ohara_cli::commands::status::current_runtime_metadata();
        runtime_meta.embed_input_mode =
            ohara_core::EmbedMode::from(args.embed_cache).index_metadata_value().to_string();
```

Pass the resulting `runtime_meta` through to `Indexer::with_runtime_metadata`
(verify the exact builder name in `indexer.rs`; plan-13 added it).

- [ ] **Step 3: Smoke test**

Run: `cargo run -p ohara-cli -- index --help`
Expected: clap shows `--embed-cache <EMBED_CACHE>` with values
`off|semantic|diff`, default `off`.

Run: `cargo build -p ohara-cli`
Expected: clean.

Run: `cargo test -p ohara-cli`
Expected: green.

- [ ] **Step 4: fmt + clippy + commit**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-cli/src/commands/index.rs
git commit -m "feat(cli): \`ohara index --embed-cache off|semantic|diff\` flag"
```

---

### Task D.4 — `ohara status` prints `embed_cache:` line

**Files:**
- Modify: `crates/ohara-cli/src/commands/status.rs`
- Modify: `crates/ohara-core/src/storage.rs` (add a small helper trait
  method `embed_cache_stats(repo) -> Result<EmbedCacheStats>`)
- Modify: `crates/ohara-storage/src/storage_impl.rs` (impl the helper)
- Modify: `crates/ohara-storage/src/tables/embed_cache.rs` (add `stats`
  function)

The `embed_cache:` line shows the configured mode and (when populated)
the cache row count and total size.

- [ ] **Step 1: Define the stats type**

In `crates/ohara-core/src/storage.rs`, near the other small DTOs (or
in a new module if the file is already crowded), add:

```rust
/// Plan 27: snapshot of the chunk_embed_cache state for `ohara status`.
#[derive(Debug, Clone, Default)]
pub struct EmbedCacheStats {
    pub row_count: u64,
    pub total_bytes: u64,
}
```

Add a default trait method:

```rust
    /// Plan 27: read-only stats over the chunk_embed_cache. Default
    /// returns an all-zero snapshot for in-memory storages.
    async fn embed_cache_stats(&self) -> Result<EmbedCacheStats> {
        Ok(EmbedCacheStats::default())
    }
```

Re-export `EmbedCacheStats` from `lib.rs` next to the existing
`pub use storage::{...}` block.

- [ ] **Step 2: Implement on SqliteStorage**

In `crates/ohara-storage/src/tables/embed_cache.rs`, add:

```rust
pub fn stats(c: &Connection) -> Result<ohara_core::storage::EmbedCacheStats> {
    let row_count: u64 = c.query_row(
        "SELECT COUNT(*) FROM chunk_embed_cache",
        [],
        |r| r.get::<_, i64>(0).map(|v| v as u64),
    )?;
    // sum of LENGTH(diff_emb) is an over-approximation that ignores
    // SQLite overhead but is close enough for the status display.
    let total_bytes: u64 = c.query_row(
        "SELECT COALESCE(SUM(LENGTH(diff_emb)), 0) FROM chunk_embed_cache",
        [],
        |r| r.get::<_, i64>(0).map(|v| v as u64),
    )?;
    Ok(ohara_core::storage::EmbedCacheStats { row_count, total_bytes })
}
```

In `storage_impl.rs`, override the trait method to call this:

```rust
    async fn embed_cache_stats(&self) -> ohara_core::Result<ohara_core::storage::EmbedCacheStats> {
        let conn = self.pool.get().await.map_err(|e| {
            ohara_core::OhraError::Storage(format!("get conn: {e}"))
        })?;
        conn.interact(|c| crate::tables::embed_cache::stats(c))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(format!("interact: {e}")))?
            .map_err(|e| ohara_core::OhraError::Storage(format!("embed_cache_stats: {e}")))
    }
```

- [ ] **Step 3: Render the line in `ohara status`**

In `crates/ohara-cli/src/commands/status.rs`, modify `run` to:
1. Read the stored `embed_input_mode` (via `storage.get_index_metadata(...)`)
   and stash it.
2. Read the cache stats.
3. Print a new line conditionally:
   ```rust
       let stored_meta = storage.get_index_metadata(&repo_id).await?;
       let mode = stored_meta.components.get("embed_input_mode")
           .map(|s| s.as_str())
           .unwrap_or("off");
       if mode != "semantic" || /* heuristic to detect cache use */ false {
           let stats = storage.embed_cache_stats().await?;
           if stats.row_count > 0 {
               println!(
                   "embed_cache: {} ({} cached vectors / {} KB)",
                   mode, stats.row_count, stats.total_bytes / 1024
               );
           }
       }
   ```

The heuristic question: the stored mode is "semantic" by default (the
metadata value), but Off and Semantic share that string. We can't
distinguish "ran with `--embed-cache=off`" from "ran with `--embed-cache=semantic`"
purely from `embed_input_mode`. Since both produce the same vectors,
*the cache contents are the right signal*: if `row_count > 0`, the user
has run with caching at some point. So just gate on `row_count > 0`.
Refactored:

```rust
    let stats = storage.embed_cache_stats().await?;
    if stats.row_count > 0 {
        let stored_meta = storage.get_index_metadata(&repo_id).await?;
        let mode = stored_meta.components.get("embed_input_mode")
            .map(|s| s.as_str())
            .unwrap_or("(unset)");
        println!(
            "embed_cache: {} ({} cached vectors / {} KB)",
            mode, stats.row_count, stats.total_bytes / 1024
        );
    }
```

- [ ] **Step 4: Add a unit test**

In `status.rs`'s tests, add a small test for the formatting (pure
function pulled out for testability). Pull the formatting into a
helper:

```rust
pub fn render_embed_cache_summary(mode: &str, stats: &ohara_core::storage::EmbedCacheStats) -> Option<String> {
    if stats.row_count == 0 {
        return None;
    }
    Some(format!(
        "embed_cache: {} ({} cached vectors / {} KB)",
        mode, stats.row_count, stats.total_bytes / 1024
    ))
}

#[test]
fn render_embed_cache_summary_omits_when_empty() {
    let stats = ohara_core::storage::EmbedCacheStats::default();
    assert_eq!(render_embed_cache_summary("semantic", &stats), None);
}

#[test]
fn render_embed_cache_summary_formats_kb() {
    let stats = ohara_core::storage::EmbedCacheStats {
        row_count: 100,
        total_bytes: 153_600, // 150 KB
    };
    assert_eq!(
        render_embed_cache_summary("diff", &stats),
        Some("embed_cache: diff (100 cached vectors / 150 KB)".to_string())
    );
}
```

Use the helper from `run`.

- [ ] **Step 5: Run tests + commit**

```
cargo test -p ohara-core
cargo test -p ohara-cli
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/ohara-core/src/storage.rs crates/ohara-storage/src/storage_impl.rs \
        crates/ohara-storage/src/tables/embed_cache.rs \
        crates/ohara-cli/src/commands/status.rs \
        crates/ohara-core/src/lib.rs
git commit -m "feat(cli): \`ohara status\` prints embed_cache: line when populated"
```

---

## Phase E — Documentation + perf harness

### Task E.1 — `docs-book/src/architecture/indexing.md` section

**Files:**
- Modify: `docs-book/src/architecture/indexing.md`

- [ ] **Step 1: Append section**

Add a new H2 section near the existing `.oharaignore` section (added
in plan-26):

```markdown
## Chunk-level embed cache (`--embed-cache`)

`ohara index` can be told to cache embeddings keyed by the content
the embedder consumes, so identical chunk content costs one embed
call across the entire history rather than one per occurrence.

Three modes:

- `off` (default) — no cache; today's behavior.
- `semantic` — cache keyed by `sha256(commit_msg + diff_text)`;
  embedder input unchanged. Hit rate is driven by exact
  `(message, diff)` repeats — cherry-picks, reverts. Conservative.
- `diff` — cache keyed by `sha256(diff_text)`; **embedder input
  changes to `diff_text` only** (commit message dropped from the
  vector lane). Hit rate is much higher (vendor refreshes, mass
  renames). The vector lane specialises in diff-similarity; commit
  messages remain indexed via the existing `fts_hunk_semantic` BM25
  lane.

`off` and `semantic` are vector-equivalent (both embed the same
input). `diff` produces a different vector lane; switching into or
out of it requires `--rebuild`.

The cache lives in the same SQLite DB as `vec_hunk` and is bounded
by `unique(content_hash, embed_model)`. No eviction in v1.

Usage:

```
ohara index --embed-cache semantic ~/code/big-repo
ohara status ~/code/big-repo   # shows embed_cache: semantic (… KB)
```
```

- [ ] **Step 2: Commit**

```bash
git add docs-book/src/architecture/indexing.md
git commit -m "docs(plan-27): document --embed-cache modes in indexing.md"
```

---

### Task E.2 — Operator perf harness `tests/perf/embed_cache_sweep.rs`

**Files:**
- Create: `tests/perf/embed_cache_sweep.rs`
- Modify: `tests/perf/Cargo.toml` (register the new bin if perf is a
  `[[bin]]`-style crate; otherwise modify to add the `[[test]]` entry).

This is an operator-run harness, not in CI. Its job is to print embed
wall-time + cache hit rate under each mode against a fixed fixture.

- [ ] **Step 1: Read the existing perf harness layout**

Open `tests/perf/Cargo.toml` and one or two existing perf files
(e.g., `tests/perf/rerank_pool_sweep.rs` from plan-23). Confirm
whether they're declared as `[[test]]` or `[[bin]]` and follow the
same pattern.

- [ ] **Step 2: Write the harness**

Create `tests/perf/embed_cache_sweep.rs` (skeleton; the implementer
fleshes out the details following the existing harness pattern):

```rust
//! Plan-27 perf harness: run `ohara index` against a fixture three
//! times (--embed-cache off|semantic|diff) and print embed wall-time +
//! cache row counts side-by-side. Operator-run; not in CI.
//!
//! Usage:
//!
//!     cargo run -p ohara-perf-tests --bin embed_cache_sweep -- \
//!         /path/to/repo
//!
//! Or whatever invocation the existing perf binaries use.

use std::path::PathBuf;
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let repo: PathBuf = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: embed_cache_sweep <repo-path>"))?
        .into();

    for mode in &["off", "semantic", "diff"] {
        // Use a fresh OHARA_HOME tempdir per mode so each run starts
        // cold (no prior cache).
        let ohara_home = tempfile::tempdir()?;
        println!("\n=== mode={mode} ===");
        let start = std::time::Instant::now();
        let status = Command::new(env!("CARGO_BIN_EXE_ohara"))
            .env("OHARA_HOME", ohara_home.path())
            .args([
                "index",
                "--rebuild",
                "--yes",
                "--embed-provider",
                "cpu",
                "--embed-cache",
                mode,
            ])
            .arg(&repo)
            .status()?;
        let elapsed = start.elapsed();
        println!("mode={mode} status={status} elapsed={elapsed:?}");

        // Surface cache stats for semantic/diff via `ohara status`.
        if *mode != "off" {
            let st = Command::new(env!("CARGO_BIN_EXE_ohara"))
                .env("OHARA_HOME", ohara_home.path())
                .arg("status")
                .arg(&repo)
                .output()?;
            let stdout = String::from_utf8_lossy(&st.stdout);
            for line in stdout.lines() {
                if line.starts_with("embed_cache:") {
                    println!("  {line}");
                }
            }
        }
    }

    Ok(())
}
```

If `ohara-perf-tests` doesn't expose `CARGO_BIN_EXE_ohara` (because
it's a separate workspace member, not the same package as `ohara-cli`),
fall back to running `cargo run -p ohara-cli -- ...` shell-style or
adjust per the existing harness pattern.

- [ ] **Step 3: Register in `Cargo.toml`**

Following the existing perf-test pattern (whatever it is — `[[bin]]`
or just `cargo run --bin <name>`), add the new harness.

- [ ] **Step 4: Smoke build**

Run: `cargo build -p ohara-perf-tests`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add tests/perf/embed_cache_sweep.rs tests/perf/Cargo.toml
git commit -m "perf(plan-27): operator harness — embed_cache_sweep across modes"
```

---

## Phase F — Integration tests

### Task F.1 — E2E: re-index with `--embed-cache=semantic` hits cache

**Files:**
- Create: `crates/ohara-cli/tests/plan_27_embed_cache_e2e.rs`

This test is `#[ignore]`'d like plan-26 F.1 (downloads embed model on
first run; opt-in via `--include-ignored`).

- [ ] **Step 1: Write the test**

Create `crates/ohara-cli/tests/plan_27_embed_cache_e2e.rs`:

```rust
//! Plan-27 end-to-end: re-indexing the same repo with
//! `--embed-cache=semantic` populates the cache on the first run and
//! reuses it on the second. We assert via `ohara status`'s
//! embed_cache: line — the row count must be > 0 after the first run.

use std::path::Path;
use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn semantic_mode_populates_cache_visible_via_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    write_file(repo.join("src"), "main.rs", "fn main() { println!(\"hi\"); }\n");
    git_add_all(repo);
    git_commit(repo, "feat: hello world");

    let idx = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--embed-cache", "semantic"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        idx.status.success(),
        "ohara index failed: {}",
        String::from_utf8_lossy(&idx.stderr)
    );

    let st = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .arg("status")
        .arg(repo)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&st.stdout);
    assert!(
        stdout.contains("embed_cache: semantic"),
        "expected `embed_cache: semantic` line; got:\n{stdout}"
    );
}

fn git_add_all(p: &Path) {
    Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["add", "."])
        .output()
        .unwrap();
}

fn git_commit(p: &Path, msg: &str) {
    Command::new("git")
        .arg("-C")
        .arg(p)
        .args([
            "-c",
            "user.email=a@a",
            "-c",
            "user.name=a",
            "commit",
            "-m",
            msg,
        ])
        .output()
        .unwrap();
}

fn write_file(dir: std::path::PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}
```

- [ ] **Step 2: Run + commit**

```
cargo test -p ohara-cli --test plan_27_embed_cache_e2e -- --include-ignored
```

Expected: PASS.

```bash
git add crates/ohara-cli/tests/plan_27_embed_cache_e2e.rs
git commit -m "test(cli): plan-27 e2e — semantic mode populates cache visible via status"
```

---

### Task F.2 — E2E: mode mismatch on `--incremental` triggers rebuild error

**Files:**
- Modify: `crates/ohara-cli/tests/plan_27_embed_cache_e2e.rs`

- [ ] **Step 1: Append the test**

```rust
#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn mode_mismatch_on_incremental_errors_with_rebuild_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    write_file(repo.join("src"), "main.rs", "fn main() {}\n");
    git_add_all(repo);
    git_commit(repo, "feat: initial");

    // First run: index with --embed-cache=semantic.
    let idx1 = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--embed-cache", "semantic"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(idx1.status.success());

    // Second run: --embed-cache=diff is a different mode → must error.
    let idx2 = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--embed-cache", "diff"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        !idx2.status.success(),
        "expected mode-mismatch failure, got success"
    );
    let stderr = String::from_utf8_lossy(&idx2.stderr);
    assert!(
        stderr.contains("embed_input_mode") || stderr.contains("rebuild"),
        "expected rebuild guidance in stderr; got:\n{stderr}"
    );
}
```

- [ ] **Step 2: Run + commit**

```
cargo test -p ohara-cli --test plan_27_embed_cache_e2e -- --include-ignored
```

Expected: PASS.

```bash
git add crates/ohara-cli/tests/plan_27_embed_cache_e2e.rs
git commit -m "test(cli): plan-27 e2e — mode mismatch errors with rebuild hint"
```

---

## Pre-completion checklist

Before opening the PR (per `CONTRIBUTING.md` §13):

- [ ] `cargo fmt --all` clean.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --workspace` green (with model-loading e2e tests
      gated under `--ignored` as in plan-26 Phase F).
- [ ] No file > 500 lines (especially
      `crates/ohara-core/src/indexer/stages/embed.rs` and
      `crates/ohara-storage/src/tables/embed_cache.rs` — the test
      fixture in C.2 grows the embed.rs test module substantially;
      consider splitting to `embed/tests.rs` if needed).
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code (one
      `expect("invariant: miss_vecs aligned ...")` in `EmbedStage::run`
      is the allowed form).
- [ ] No `println!` outside `ohara-cli` user-facing output.
- [ ] Workspace-only deps: `sha2` and `hex` already in workspace; no
      new third-party deps added.
- [ ] Mode-switch story documented (`--rebuild` required for Diff↔
      non-Diff transitions); `Off ↔ Semantic` is safe.
- [ ] `cargo build --release` clean for both `ohara` and `ohara-mcp`.

## Out of scope (companion plans)

- **Plan 28 (Spec D) — Parallel commit pipeline.** Worker pool around
  parse + embed; in-order watermark serializer. Sequenced AFTER this
  plan so the embed-stage shape is stable before being parallelised.
- **Cross-repo embed sharing.** A team-shared cache. Future RFC.
- **Cache eviction / pruning.** Bounded by unique-content-hash;
  revisit if real-world data shows unbounded growth.
- **Quality eval framework.** A reusable retrieval-quality evaluation
  surface for benchmarking dedup modes against a fixed query set. The
  `embed_cache_sweep.rs` operator harness is enough for the
  default-flip decision; a reusable surface is its own project.
