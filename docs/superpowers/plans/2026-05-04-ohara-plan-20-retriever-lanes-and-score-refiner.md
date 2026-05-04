# ohara plan-20 — Retriever lanes + ScoreRefiner trait

> **Status:** draft

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per
> repo conventions: commit after each red test and again after each
> green implementation.

**Goal:** refactor `ohara_core::retriever::Retriever::find_pattern_with_profile`
(currently ~330 lines with all four retrieval lanes, RRF, rerank, and recency
inlined as a flat body) into a `RetrievalLane` trait with four implementations
and a separate `ScoreRefiner` trait for rerank and recency. Profile-gating moves
into each lane so disabled lanes self-skip rather than having the orchestrator
branch on profile flags. New lanes and refiners plug in without touching the
coordinator.

**Architecture:** new module subtree under `crates/ohara-core/src/retriever/`:

- `lanes/vec.rs` — `VecLane`, wraps `Arc<dyn EmbeddingProvider>`.
- `lanes/bm25_text.rs` — `Bm25TextLane`, drives `Storage::bm25_hunks_by_text`.
- `lanes/bm25_hist_sym.rs` — `Bm25HistSymLane`, drives
  `Storage::bm25_hunks_by_historical_symbol`.
- `lanes/bm25_head_sym.rs` — `Bm25HeadSymLane`, drives
  `Storage::bm25_hunks_by_symbol_name`.
- `refiners/cross_encoder.rs` — `CrossEncoderRefiner`, wraps `Arc<dyn
  RerankProvider>`.
- `refiners/recency.rs` — `RecencyRefiner`, the half-life multiplier.
- `coordinator.rs` — `Coordinator::run`: `join_all` lanes, RRF-merge
  (free function, not a trait), apply refiners in sequence, truncate.

The `Retriever` struct keeps its public API unchanged. Internally it
constructs the lanes and refiners once in `Retriever::new` / builder
methods and delegates to `coordinator.rs`.

**Tech Stack:** Rust 2021, `async-trait`, `futures::future::join_all`, existing
`tokio` runtime, existing `crate::storage::Storage` / `crate::embed`
traits.

**Spec:** none — internal refactor. Not driven by a published design doc;
the motivation is `retriever.rs` exceeding the 500-line file limit mandated
by `CONTRIBUTING.md §5`.

**Scope check:** plan-20 is `ohara-core` only. No SQL changes, no new
storage trait methods, no changes to `ohara-storage`, `ohara-embed`, or
the binary crates. The public `Retriever` API (including
`find_pattern_with_profile`, `with_reranker`, `with_weights`) stays
unchanged. Existing retriever tests pass without modification after Phase D.

---

## Phase A — Trait definitions

Define the two trait contracts and the `LaneId` enum. Each task ships with
a toy impl + test confirming the trait is object-safe and wires through a
`Box<dyn …>`.

### Task A.1 — `RetrievalLane` trait + `LaneId` enum + `is_lane_enabled`

**Files:**
- Create: `crates/ohara-core/src/retriever/lanes/mod.rs`
- Modify: `crates/ohara-core/src/retriever.rs` (add `pub mod lanes;` declaration
  and re-export `LaneId`, `RetrievalLane`)
- Modify: `crates/ohara-core/src/query_understanding.rs` (add `is_lane_enabled`)

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)]` module at the bottom of the new file
`crates/ohara-core/src/retriever/lanes/mod.rs` before any trait
implementation exists:

```rust
#[cfg(test)]
mod trait_object_tests {
    use super::*;
    use crate::query::{PatternQuery};
    use crate::storage::HunkHit;
    use crate::types::RepoId;
    use async_trait::async_trait;

    struct DummyLane(LaneId);

    #[async_trait]
    impl RetrievalLane for DummyLane {
        fn id(&self) -> LaneId {
            self.0
        }
        async fn search(
            &self,
            _query: &PatternQuery,
            _repo_id: &RepoId,
            _k: usize,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn retrieval_lane_is_object_safe() {
        let lane: Box<dyn RetrievalLane> = Box::new(DummyLane(LaneId::Vec));
        assert_eq!(lane.id(), LaneId::Vec);
        let q = PatternQuery {
            query: "test".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &id, 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn lane_id_variants_are_distinct() {
        assert_ne!(LaneId::Vec, LaneId::Bm25Text);
        assert_ne!(LaneId::Bm25Text, LaneId::Bm25HistSym);
        assert_ne!(LaneId::Bm25HistSym, LaneId::Bm25HeadSym);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core retrieval_lane_is_object_safe -- --nocapture
```

Expected: FAIL — `lanes/mod.rs` does not yet exist; the crate doesn't
compile.

- [ ] **Step 3: Create the module file with the trait and enum**

Create `crates/ohara-core/src/retriever/lanes/mod.rs`:

```rust
//! Plan 20 — retrieval lane abstractions.
//!
//! Each lane encapsulates one candidate-gathering strategy (vector KNN,
//! BM25-by-text, BM25-by-historical-symbol, BM25-by-head-symbol). The
//! coordinator fires all enabled lanes via `join_all` and merges their
//! results with Reciprocal Rank Fusion.
//!
//! Lane implementations live in sibling modules:
//!   vec, bm25_text, bm25_hist_sym, bm25_head_sym.

use crate::query::PatternQuery;
use crate::storage::HunkHit;
use crate::types::RepoId;
use async_trait::async_trait;

pub mod bm25_head_sym;
pub mod bm25_hist_sym;
pub mod bm25_text;
pub mod vec;

/// Stable identifier for each retrieval lane. Used by
/// `RetrievalProfile::is_lane_enabled` so the coordinator can ask each lane
/// whether its profile flag is set without knowing the concrete type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneId {
    Vec,
    Bm25Text,
    Bm25HistSym,
    Bm25HeadSym,
}

/// One retrieval strategy.
///
/// Implementors query their respective storage method and return an ordered
/// `Vec<HunkHit>` from most to least relevant according to that lane's
/// scoring function. The caller (coordinator) merges lanes via RRF —
/// lane-internal scores are used only for the informational
/// `HunkHit::similarity` field.
///
/// Each implementation checks
/// `query.profile.is_lane_enabled(self.id())` as its first step and returns
/// `Ok(vec![])` when the lane is disabled by the profile (option a — lanes
/// self-gate). This keeps the coordinator dumb: it always fires all lanes
/// via `join_all` and trusts disabled lanes to return empty without touching
/// storage.
#[async_trait]
pub trait RetrievalLane: Send + Sync {
    fn id(&self) -> LaneId;
    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>>;
}
```

- [ ] **Step 4: Add `pub mod lanes;` to `retriever.rs` and re-export**

In `crates/ohara-core/src/retriever.rs`, add near the top (before the
`use` block for this file's internals):

```rust
pub mod lanes;
pub use lanes::{LaneId, RetrievalLane};
```

Also add `pub mod retriever;` as a *directory* module declaration in
`crates/ohara-core/src/lib.rs` — this requires converting `retriever.rs`
to `retriever/mod.rs`. See note below.

> **Conversion note:** `retriever.rs` becomes `retriever/mod.rs` when the
> first submodule is added. Move the file:
> `mv crates/ohara-core/src/retriever.rs crates/ohara-core/src/retriever/mod.rs`
> Nothing else changes — Rust's module system resolves both forms
> identically from the call sites' perspective.

- [ ] **Step 5: Add `is_lane_enabled` to `RetrievalProfile`**

In `crates/ohara-core/src/query_understanding.rs`, add after the existing
`for_intent` method in the `impl RetrievalProfile` block:

```rust
    /// Return whether a given lane is enabled for this profile.
    ///
    /// Plan 20: `RetrievalLane` impls call this method as their first
    /// step and return `Ok(vec![])` when the result is `false`. This
    /// keeps the coordinator dumb — it fires all lanes unconditionally.
    pub fn is_lane_enabled(&self, lane: crate::retriever::LaneId) -> bool {
        match lane {
            crate::retriever::LaneId::Vec => self.vec_lane_enabled,
            crate::retriever::LaneId::Bm25Text => self.text_lane_enabled,
            crate::retriever::LaneId::Bm25HistSym => self.symbol_lane_enabled,
            crate::retriever::LaneId::Bm25HeadSym => self.symbol_lane_enabled,
        }
    }
```

- [ ] **Step 6: Run the tests**

```
cargo test -p ohara-core
```

Expected: all existing tests pass; new `trait_object_tests` pass.

- [ ] **Step 7: Commit (red test then green implementation)**

```bash
git add crates/ohara-core/src/retriever/ crates/ohara-core/src/query_understanding.rs
git commit -m "test(core): plan-20 RetrievalLane trait + LaneId contract (failing)"
```

Then after implementing:

```bash
git add crates/ohara-core/src/retriever/ crates/ohara-core/src/query_understanding.rs
git commit -m "feat(core): plan-20 RetrievalLane trait, LaneId enum, is_lane_enabled"
```

---

### Task A.2 — `ScoreRefiner` trait

**Files:**
- Create: `crates/ohara-core/src/retriever/refiners/mod.rs`
- Modify: `crates/ohara-core/src/retriever/mod.rs` (add `pub mod refiners;` and
  re-export `ScoreRefiner`)

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/refiners/mod.rs` with a test only
(no implementation yet):

```rust
#[cfg(test)]
mod trait_object_tests {
    use super::*;
    use crate::storage::HunkHit;
    use async_trait::async_trait;

    struct PassthroughRefiner;

    #[async_trait]
    impl ScoreRefiner for PassthroughRefiner {
        async fn refine(
            &self,
            _query_text: &str,
            hits: Vec<HunkHit>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn score_refiner_is_object_safe() {
        let refiner: Box<dyn ScoreRefiner> = Box::new(PassthroughRefiner);
        let hits: Vec<HunkHit> = vec![];
        let out = refiner.refine("q", hits).await.unwrap();
        assert!(out.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core score_refiner_is_object_safe -- --nocapture
```

Expected: FAIL — `ScoreRefiner` does not exist.

- [ ] **Step 3: Implement the module**

Add the trait definition above the test module in
`crates/ohara-core/src/retriever/refiners/mod.rs`:

```rust
//! Plan 20 — post-RRF score refiners.
//!
//! A `ScoreRefiner` takes the RRF-merged `Vec<HunkHit>` and returns a
//! reordered/rescored version. The coordinator applies a sequence of
//! refiners in order:
//!
//! ```text
//! for refiner in refiners {
//!     hits = refiner.refine(query_text, hits).await?;
//! }
//! ```
//!
//! Implementations live in sibling modules:
//!   cross_encoder, recency.

use crate::storage::HunkHit;
use async_trait::async_trait;

pub mod cross_encoder;
pub mod recency;

/// One post-RRF transformation step.
///
/// Refiners receive the full ordered candidate list and return a new
/// ordered list. They may reorder, rescore, or prune candidates.
/// Returning the list in the same order is a valid (no-op) implementation.
///
/// The `query_text` parameter is the raw query string. Cross-encoder
/// refiners use it for relevance scoring; recency refiners ignore it.
#[async_trait]
pub trait ScoreRefiner: Send + Sync {
    async fn refine(
        &self,
        query_text: &str,
        hits: Vec<HunkHit>,
    ) -> crate::Result<Vec<HunkHit>>;
}
```

- [ ] **Step 4: Add `pub mod refiners;` to `retriever/mod.rs`**

```rust
pub mod refiners;
pub use refiners::ScoreRefiner;
```

- [ ] **Step 5: Run the tests**

```
cargo test -p ohara-core
```

Expected: all tests pass including the new `score_refiner_is_object_safe`.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/retriever/refiners/
git commit -m "test(core): plan-20 ScoreRefiner trait contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/refiners/
git commit -m "feat(core): plan-20 ScoreRefiner trait"
```

---

## Phase B — Lane extraction (4 tasks)

Each task lifts the relevant inline block from today's
`find_pattern_with_profile` into its own module. After each task, the
existing tests in `retriever/mod.rs` must remain green (the orchestrator
still uses the old inline code — Phase D swaps it in).

### Task B.1 — `VecLane`

**Files:**
- Create: `crates/ohara-core/src/retriever/lanes/vec.rs`

**Test strategy:** construct a `VecLane` with a fake embedder (returns
all-zero vectors) and a fake storage (returns a canned `Vec<HunkHit>`
when `knn_hunks` is called), and assert the lane returns that hit.

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/lanes/vec.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{PatternQuery};
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;
    use std::sync::Arc;

    // Minimal fake storage: returns a preset hit list for knn_hunks.
    struct KnnStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for KnnStorage {
        // Only knn_hunks needs an actual implementation.
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
        }
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus { last_indexed_commit: None, commits_behind_head: 0, indexed_at: None })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &crate::storage::CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn put_hunks(&self, _: &RepoId, _: &[crate::storage::HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> { Ok(()) }
        async fn bm25_hunks_by_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_symbol_name(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_historical_symbol(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> crate::Result<Vec<crate::types::HunkSymbol>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<crate::types::CommitMeta>> { Ok(None) }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<Vec<crate::types::Hunk>> { Ok(vec![]) }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> { Ok(vec![]) }
        async fn get_index_metadata(&self, _: &RepoId) -> crate::Result<crate::index_metadata::StoredIndexMetadata> { Ok(crate::index_metadata::StoredIndexMetadata::default()) }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> { Ok(()) }
    }

    struct ZeroEmbedder;
    #[async_trait]
    impl crate::EmbeddingProvider for ZeroEmbedder {
        fn dimension(&self) -> usize { 4 }
        fn model_id(&self) -> &str { "zero" }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "abc".into(),
                file_path: "src/lib.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn foo() {}".into(),
            },
            commit: CommitMeta {
                commit_sha: "abc".into(),
                parent_sha: None,
                is_merge: false,
                author: Some("alice".into()),
                ts: 1_700_000_000,
                message: "add foo".into(),
            },
            similarity: 0.9,
        }
    }

    #[tokio::test]
    async fn vec_lane_returns_knn_hits() {
        let hit = make_hit(1);
        let storage: Arc<dyn crate::Storage> = Arc::new(KnnStorage(vec![hit.clone()]));
        let embedder: Arc<dyn crate::EmbeddingProvider> = Arc::new(ZeroEmbedder);
        let lane = VecLane::new(storage, embedder);

        let q = PatternQuery {
            query: "retry".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 1);
    }

    #[tokio::test]
    async fn vec_lane_self_skips_when_profile_disables_it() {
        // Override the query so its profile has vec disabled.
        // We embed the profile gate check: when is_lane_enabled(Vec)
        // returns false, search returns Ok(vec![]) without calling knn.
        // The KnnStorage would return a hit — if the lane fires, the
        // test fails.
        use crate::query_understanding::RetrievalProfile;

        let hit = make_hit(2);
        let storage: Arc<dyn crate::Storage> = Arc::new(KnnStorage(vec![hit]));
        let embedder: Arc<dyn crate::EmbeddingProvider> = Arc::new(ZeroEmbedder);
        let lane = VecLane::new(storage, embedder);

        // Build a PatternQuery whose effective profile disables Vec.
        // The lane reads query.profile; for this test we construct a
        // profile-annotated query directly using the internal helper.
        let q = PatternQuery {
            query: "config env".into(), // classified -> Configuration, symbol_lane disabled
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        // Configuration profile disables symbol lane but not vec —
        // we need an explicit disabled-vec profile for this test.
        // VecLane reads `is_lane_enabled(LaneId::Vec)` which checks
        // `profile.vec_lane_enabled`. Construct a profile inline and
        // pass it via the extended query form introduced in Phase D.
        // For Phase B, use the lane's `search_with_profile` variant
        // (added in this task) that accepts an explicit profile so
        // unit tests can inject arbitrary profiles without going
        // through parse_query.
        let profile = RetrievalProfile {
            name: "test".into(),
            recency_multiplier: 1.0,
            vec_lane_enabled: false,      // <-- disabled
            text_lane_enabled: true,
            symbol_lane_enabled: true,
            rerank_top_k: None,
            explanation: "test".into(),
        };
        let hits = lane.search_with_profile(&q, &repo_id, 10, &profile).await.unwrap();
        assert!(hits.is_empty(), "vec lane must self-skip when vec_lane_enabled=false");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core vec_lane_returns_knn_hits -- --nocapture
```

Expected: FAIL — `VecLane` does not exist.

- [ ] **Step 3: Implement `VecLane`**

Add the implementation above the test module in
`crates/ohara-core/src/retriever/lanes/vec.rs`:

```rust
//! Plan 20 — vector-KNN retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::embed::EmbeddingProvider;
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

/// Retrieval lane: vector KNN on hunk embeddings.
///
/// Embeds the query text once per `search` call using the injected
/// `EmbeddingProvider`. The embed step is inside the lane so the
/// coordinator does not need to know which lanes require embeddings.
pub struct VecLane {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn EmbeddingProvider>,
}

impl VecLane {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self { storage, embedder }
    }

    /// Profile-parameterised search, used in unit tests to inject
    /// an explicit `RetrievalProfile` without going through `parse_query`.
    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Vec) {
            return Ok(vec![]);
        }
        let q_text = vec![query.query.clone()];
        let mut embs = self.embedder.embed_batch(&q_text).await?;
        let q_emb = embs
            .pop()
            .ok_or_else(|| crate::OhraError::Embedding("embed_batch returned empty".into()))?;
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        self.storage
            .knn_hunks(
                repo_id,
                &q_emb,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            )
            .await
    }
}

#[async_trait]
impl RetrievalLane for VecLane {
    fn id(&self) -> LaneId {
        LaneId::Vec
    }

    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>> {
        let profile = crate::query_understanding::RetrievalProfile::for_intent(
            crate::query_understanding::parse_query(&query.query).intent,
        );
        self.search_with_profile(query, repo_id, k, &profile).await
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -p ohara-core -- vec_lane
```

Expected: both `vec_lane_returns_knn_hits` and
`vec_lane_self_skips_when_profile_disables_it` pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/lanes/vec.rs
git commit -m "test(core): plan-20 VecLane contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/lanes/vec.rs
git commit -m "feat(core): plan-20 VecLane — vec-KNN retrieval lane"
```

---

### Task B.2 — `Bm25TextLane`

**Files:**
- Create: `crates/ohara-core/src/retriever/lanes/bm25_text.rs`

**Test strategy:** fake storage returns a canned hit from
`bm25_hunks_by_text`; assert the lane returns it. Profile with
`text_lane_enabled: false` returns empty.

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/lanes/bm25_text.rs` with tests
only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct Bm25TextStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for Bm25TextStorage {
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
        }
        // All other methods: delegate to the same no-op stubs as
        // Task B.1. Repeat the same stub block here.
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus { last_indexed_commit: None, commits_behind_head: 0, indexed_at: None })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &crate::storage::CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn put_hunks(&self, _: &RepoId, _: &[crate::storage::HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> { Ok(()) }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_symbol_name(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_historical_symbol(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> crate::Result<Vec<crate::types::HunkSymbol>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<crate::types::CommitMeta>> { Ok(None) }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<Vec<crate::types::Hunk>> { Ok(vec![]) }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> { Ok(vec![]) }
        async fn get_index_metadata(&self, _: &RepoId) -> crate::Result<crate::index_metadata::StoredIndexMetadata> { Ok(crate::index_metadata::StoredIndexMetadata::default()) }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> { Ok(()) }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "bbb".into(),
                file_path: "src/lib.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Modified,
                diff_text: "+fn bar() {}".into(),
            },
            commit: CommitMeta {
                commit_sha: "bbb".into(),
                parent_sha: None,
                is_merge: false,
                author: Some("bob".into()),
                ts: 1_700_000_000,
                message: "add bar".into(),
            },
            similarity: 0.7,
        }
    }

    #[tokio::test]
    async fn bm25_text_lane_returns_fts_hits() {
        let hit = make_hit(10);
        let storage: Arc<dyn crate::Storage> = Arc::new(Bm25TextStorage(vec![hit.clone()]));
        let lane = Bm25TextLane::new(storage);

        let q = PatternQuery {
            query: "retry backoff".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 10);
    }

    #[tokio::test]
    async fn bm25_text_lane_self_skips_when_text_disabled() {
        use crate::query_understanding::RetrievalProfile;
        let hit = make_hit(11);
        let storage: Arc<dyn crate::Storage> = Arc::new(Bm25TextStorage(vec![hit]));
        let lane = Bm25TextLane::new(storage);

        let profile = RetrievalProfile {
            name: "test".into(),
            recency_multiplier: 1.0,
            vec_lane_enabled: true,
            text_lane_enabled: false,   // disabled
            symbol_lane_enabled: true,
            rerank_top_k: None,
            explanation: "test".into(),
        };
        let q = PatternQuery {
            query: "retry".into(), k: 5, language: None, since_unix: None, no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search_with_profile(&q, &repo_id, 10, &profile).await.unwrap();
        assert!(hits.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core bm25_text_lane_returns_fts_hits -- --nocapture
```

Expected: FAIL.

- [ ] **Step 3: Implement `Bm25TextLane`**

Add above the test module in `crates/ohara-core/src/retriever/lanes/bm25_text.rs`:

```rust
//! Plan 20 — BM25-by-text retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

pub struct Bm25TextLane {
    storage: Arc<dyn Storage>,
}

impl Bm25TextLane {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Bm25Text) {
            return Ok(vec![]);
        }
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        self.storage
            .bm25_hunks_by_text(
                repo_id,
                &query.query,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            )
            .await
    }
}

#[async_trait]
impl RetrievalLane for Bm25TextLane {
    fn id(&self) -> LaneId {
        LaneId::Bm25Text
    }

    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>> {
        let profile = crate::query_understanding::RetrievalProfile::for_intent(
            crate::query_understanding::parse_query(&query.query).intent,
        );
        self.search_with_profile(query, repo_id, k, &profile).await
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -- bm25_text_lane
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_text.rs
git commit -m "test(core): plan-20 Bm25TextLane contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_text.rs
git commit -m "feat(core): plan-20 Bm25TextLane — BM25-by-text lane"
```

---

### Task B.3 — `Bm25HistSymLane`

**Files:**
- Create: `crates/ohara-core/src/retriever/lanes/bm25_hist_sym.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/lanes/bm25_hist_sym.rs` with a
test that a fake storage returning one hit from
`bm25_hunks_by_historical_symbol` is surfaced by the lane, and that
`symbol_lane_enabled: false` causes the lane to self-skip:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct HistSymStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for HistSymStorage {
        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
        }
        // Same no-op stubs as B.1 for all other methods.
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus { last_indexed_commit: None, commits_behind_head: 0, indexed_at: None })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &crate::storage::CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn put_hunks(&self, _: &RepoId, _: &[crate::storage::HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> { Ok(()) }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_symbol_name(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> crate::Result<Vec<crate::types::HunkSymbol>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<crate::types::CommitMeta>> { Ok(None) }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<Vec<crate::types::Hunk>> { Ok(vec![]) }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> { Ok(vec![]) }
        async fn get_index_metadata(&self, _: &RepoId) -> crate::Result<crate::index_metadata::StoredIndexMetadata> { Ok(crate::index_metadata::StoredIndexMetadata::default()) }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> { Ok(()) }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "ccc".into(), file_path: "src/lib.rs".into(), language: Some("rust".into()), change_kind: ChangeKind::Added, diff_text: "+fn baz(){}".into() },
            commit: CommitMeta { commit_sha: "ccc".into(), parent_sha: None, is_merge: false, author: Some("charlie".into()), ts: 1_700_000_000, message: "add baz".into() },
            similarity: 0.6,
        }
    }

    #[tokio::test]
    async fn bm25_hist_sym_lane_returns_hits() {
        let hit = make_hit(20);
        let storage: Arc<dyn crate::Storage> = Arc::new(HistSymStorage(vec![hit]));
        let lane = Bm25HistSymLane::new(storage);
        let q = PatternQuery { query: "baz".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 20);
    }

    #[tokio::test]
    async fn bm25_hist_sym_lane_self_skips_when_symbol_disabled() {
        use crate::query_understanding::RetrievalProfile;
        let hit = make_hit(21);
        let storage: Arc<dyn crate::Storage> = Arc::new(HistSymStorage(vec![hit]));
        let lane = Bm25HistSymLane::new(storage);
        let profile = RetrievalProfile { name: "test".into(), recency_multiplier: 1.0, vec_lane_enabled: true, text_lane_enabled: true, symbol_lane_enabled: false, rerank_top_k: None, explanation: "test".into() };
        let q = PatternQuery { query: "baz".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search_with_profile(&q, &repo_id, 10, &profile).await.unwrap();
        assert!(hits.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core bm25_hist_sym_lane_returns_hits -- --nocapture
```

- [ ] **Step 3: Implement `Bm25HistSymLane`**

Add the implementation above the test module:

```rust
//! Plan 20 — BM25-by-historical-symbol retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

pub struct Bm25HistSymLane {
    storage: Arc<dyn Storage>,
}

impl Bm25HistSymLane {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Bm25HistSym) {
            return Ok(vec![]);
        }
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        self.storage
            .bm25_hunks_by_historical_symbol(
                repo_id,
                &query.query,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            )
            .await
    }
}

#[async_trait]
impl RetrievalLane for Bm25HistSymLane {
    fn id(&self) -> LaneId {
        LaneId::Bm25HistSym
    }

    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>> {
        let profile = crate::query_understanding::RetrievalProfile::for_intent(
            crate::query_understanding::parse_query(&query.query).intent,
        );
        self.search_with_profile(query, repo_id, k, &profile).await
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -- bm25_hist_sym_lane
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_hist_sym.rs
git commit -m "test(core): plan-20 Bm25HistSymLane contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_hist_sym.rs
git commit -m "feat(core): plan-20 Bm25HistSymLane — historical-symbol BM25 lane"
```

---

### Task B.4 — `Bm25HeadSymLane`

**Files:**
- Create: `crates/ohara-core/src/retriever/lanes/bm25_head_sym.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/lanes/bm25_head_sym.rs` with a
test that `bm25_hunks_by_symbol_name` results are returned, and that
`symbol_lane_enabled: false` triggers self-skip (same pattern as B.3,
different storage method).

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct HeadSymStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for HeadSymStorage {
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
        }
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus { last_indexed_commit: None, commits_behind_head: 0, indexed_at: None })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &crate::storage::CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn put_hunks(&self, _: &RepoId, _: &[crate::storage::HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> { Ok(()) }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_historical_symbol(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> crate::Result<Vec<crate::types::HunkSymbol>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<crate::types::CommitMeta>> { Ok(None) }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<Vec<crate::types::Hunk>> { Ok(vec![]) }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> { Ok(vec![]) }
        async fn get_index_metadata(&self, _: &RepoId) -> crate::Result<crate::index_metadata::StoredIndexMetadata> { Ok(crate::index_metadata::StoredIndexMetadata::default()) }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> crate::Result<()> { Ok(()) }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "ddd".into(), file_path: "src/main.rs".into(), language: Some("rust".into()), change_kind: ChangeKind::Modified, diff_text: "+fn qux(){}".into() },
            commit: CommitMeta { commit_sha: "ddd".into(), parent_sha: None, is_merge: false, author: Some("diana".into()), ts: 1_700_000_000, message: "add qux".into() },
            similarity: 0.5,
        }
    }

    #[tokio::test]
    async fn bm25_head_sym_lane_returns_hits() {
        let hit = make_hit(30);
        let storage: Arc<dyn crate::Storage> = Arc::new(HeadSymStorage(vec![hit]));
        let lane = Bm25HeadSymLane::new(storage);
        let q = PatternQuery { query: "qux".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 30);
    }

    #[tokio::test]
    async fn bm25_head_sym_lane_self_skips_when_symbol_disabled() {
        use crate::query_understanding::RetrievalProfile;
        let hit = make_hit(31);
        let storage: Arc<dyn crate::Storage> = Arc::new(HeadSymStorage(vec![hit]));
        let lane = Bm25HeadSymLane::new(storage);
        let profile = RetrievalProfile { name: "test".into(), recency_multiplier: 1.0, vec_lane_enabled: true, text_lane_enabled: true, symbol_lane_enabled: false, rerank_top_k: None, explanation: "test".into() };
        let q = PatternQuery { query: "qux".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search_with_profile(&q, &repo_id, 10, &profile).await.unwrap();
        assert!(hits.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core bm25_head_sym_lane_returns_hits -- --nocapture
```

- [ ] **Step 3: Implement `Bm25HeadSymLane`**

```rust
//! Plan 20 — BM25-by-head-symbol retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

pub struct Bm25HeadSymLane {
    storage: Arc<dyn Storage>,
}

impl Bm25HeadSymLane {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Bm25HeadSym) {
            return Ok(vec![]);
        }
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        self.storage
            .bm25_hunks_by_symbol_name(
                repo_id,
                &query.query,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            )
            .await
    }
}

#[async_trait]
impl RetrievalLane for Bm25HeadSymLane {
    fn id(&self) -> LaneId {
        LaneId::Bm25HeadSym
    }

    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>> {
        let profile = crate::query_understanding::RetrievalProfile::for_intent(
            crate::query_understanding::parse_query(&query.query).intent,
        );
        self.search_with_profile(query, repo_id, k, &profile).await
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -- bm25_head_sym_lane
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_head_sym.rs
git commit -m "test(core): plan-20 Bm25HeadSymLane contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/lanes/bm25_head_sym.rs
git commit -m "feat(core): plan-20 Bm25HeadSymLane — head-symbol BM25 lane"
```

---

## Phase C — Refiner extraction (2 tasks)

### Task C.1 — `CrossEncoderRefiner`

**Files:**
- Create: `crates/ohara-core/src/retriever/refiners/cross_encoder.rs`

The reranker is currently behind a `Semaphore(1)` in the daemon (plan-16
spec). The `CrossEncoderRefiner` wraps `Arc<dyn RerankProvider>` and does
not own a semaphore itself — the daemon's call site holds the semaphore
around the entire `find_pattern_with_profile` call as it does today. The
orchestrator is the natural place to hold the semaphore during the refiner
step; `CrossEncoderRefiner::refine` therefore makes a plain `rerank` call
without internal synchronisation. This keeps the refiner stateless and the
semaphore policy at the call site, consistent with current behaviour.

**Test strategy:** construct a `CrossEncoderRefiner` with a
`ScriptedReranker` (returns scores `[2.0, 1.0, 3.0]` for three inputs);
assert the three hits are reordered to `[2, 0, 1]` (score-descending).

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/refiners/cross_encoder.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::RerankProvider;
    use crate::storage::{HunkHit, HunkId};
    use async_trait::async_trait;
    use std::sync::Arc;

    fn make_hit(id: HunkId, diff: &str) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "x".into(), file_path: "f.rs".into(), language: None, change_kind: ChangeKind::Added, diff_text: diff.into() },
            commit: CommitMeta { commit_sha: "x".into(), parent_sha: None, is_merge: false, author: None, ts: 0, message: "m".into() },
            similarity: 0.5,
        }
    }

    struct ScriptedReranker(Vec<f32>);

    #[async_trait]
    impl RerankProvider for ScriptedReranker {
        async fn rerank(&self, _: &str, candidates: &[&str]) -> crate::Result<Vec<f32>> {
            assert_eq!(candidates.len(), self.0.len(), "score count mismatch");
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn cross_encoder_refiner_reorders_by_score() {
        // Three hits; scripted reranker gives scores [2.0, 1.0, 3.0].
        // After refine, order must be [id=2 (score 3.0), id=0 (score 2.0), id=1 (score 1.0)].
        let hits = vec![
            make_hit(100, "diff-a"),
            make_hit(101, "diff-b"),
            make_hit(102, "diff-c"),
        ];
        let reranker: Arc<dyn RerankProvider> =
            Arc::new(ScriptedReranker(vec![2.0, 1.0, 3.0]));
        let refiner = CrossEncoderRefiner::new(reranker);
        let out = refiner.refine("query", hits).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].hunk_id, 102, "highest score (3.0) must be first");
        assert_eq!(out[1].hunk_id, 100, "second score (2.0) must be second");
        assert_eq!(out[2].hunk_id, 101, "lowest score (1.0) must be last");
    }

    #[tokio::test]
    async fn cross_encoder_refiner_empty_input_returns_empty() {
        let reranker: Arc<dyn RerankProvider> =
            Arc::new(ScriptedReranker(vec![]));
        let refiner = CrossEncoderRefiner::new(reranker);
        let out = refiner.refine("q", vec![]).await.unwrap();
        assert!(out.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core cross_encoder_refiner_reorders_by_score -- --nocapture
```

Expected: FAIL.

- [ ] **Step 3: Implement `CrossEncoderRefiner`**

Add above the test module:

```rust
//! Plan 20 — cross-encoder rerank refiner.

use super::ScoreRefiner;
use crate::embed::RerankProvider;
use crate::storage::HunkHit;
use async_trait::async_trait;
use std::sync::Arc;

/// Reranks candidates with an injected `RerankProvider` (BGE-reranker-base
/// in production). The refiner does not own a semaphore — the caller
/// (coordinator or daemon) holds one around the full pipeline step if
/// needed, as it does today.
pub struct CrossEncoderRefiner {
    reranker: Arc<dyn RerankProvider>,
}

impl CrossEncoderRefiner {
    pub fn new(reranker: Arc<dyn RerankProvider>) -> Self {
        Self { reranker }
    }
}

#[async_trait]
impl ScoreRefiner for CrossEncoderRefiner {
    async fn refine(
        &self,
        query_text: &str,
        hits: Vec<HunkHit>,
    ) -> crate::Result<Vec<HunkHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        let candidates: Vec<&str> = hits.iter().map(|h| h.hunk.diff_text.as_str()).collect();
        let scores = self.reranker.rerank(query_text, &candidates).await?;
        // Zip hits with scores, sort descending by score, discard scores.
        let mut scored: Vec<(HunkHit, f32)> = hits.into_iter().zip(scores).collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(scored.into_iter().map(|(h, _)| h).collect())
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -- cross_encoder_refiner
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/refiners/cross_encoder.rs
git commit -m "test(core): plan-20 CrossEncoderRefiner contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/refiners/cross_encoder.rs
git commit -m "feat(core): plan-20 CrossEncoderRefiner — post-RRF rerank refiner"
```

---

### Task C.2 — `RecencyRefiner`

**Files:**
- Create: `crates/ohara-core/src/retriever/refiners/recency.rs`

**Test strategy:** two hits with timestamps 1 day vs. 100 days old
(relative to a fixed `now_unix`). Without a reranker both have base score
1.0. After `RecencyRefiner::refine`, the 1-day-old hit must rank higher
because its recency multiplier is larger.

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/retriever/refiners/recency.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::RankingWeights;
    use crate::storage::{HunkHit, HunkId};

    fn make_hit(id: HunkId, ts: i64) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "x".into(), file_path: "f.rs".into(), language: None, change_kind: ChangeKind::Added, diff_text: "diff".into() },
            commit: CommitMeta { commit_sha: "x".into(), parent_sha: None, is_merge: false, author: None, ts, message: "m".into() },
            similarity: 1.0,
        }
    }

    #[tokio::test]
    async fn recency_refiner_newer_hit_ranks_higher() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        // id=1 is 1 day old, id=2 is 100 days old.
        // Both have similarity=1.0 (simulating equal rerank scores).
        // After recency, id=1 must be first.
        let hits = vec![
            make_hit(2, now - 100 * day), // older — given first to show ordering changes
            make_hit(1, now - day),       // newer
        ];
        let weights = RankingWeights::default();
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].hunk_id, 1,
            "newer hit (1 day old) must outrank older (100 days old)"
        );
        assert_eq!(out[1].hunk_id, 2);
    }

    #[tokio::test]
    async fn recency_refiner_zero_weight_preserves_input_order() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(10, now - day),
            make_hit(11, now - 200 * day),
        ];
        let mut weights = RankingWeights::default();
        weights.recency_weight = 0.0; // disable tie-break
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        // With recency_weight=0 both get score similarity*(1+0*exp) = 1.0,
        // sort is stable so input order is preserved.
        assert_eq!(out[0].hunk_id, 10);
        assert_eq!(out[1].hunk_id, 11);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core recency_refiner_newer_hit_ranks_higher -- --nocapture
```

Expected: FAIL.

- [ ] **Step 3: Implement `RecencyRefiner`**

```rust
//! Plan 20 — recency multiplier refiner.
//!
//! Applies the half-life exp-decay recency factor to each hit's
//! `similarity` score and re-sorts. This is the same formula used
//! in the pre-plan-20 inline `find_pattern_with_profile`:
//!
//! ```text
//! final = similarity * (1.0 + recency_weight * exp(-age_days / half_life_days))
//! ```
//!
//! The `profile.recency_multiplier` nudge (plan 12) is applied by the
//! coordinator before constructing this refiner: it multiplies
//! `RankingWeights::recency_weight` by `profile.recency_multiplier`
//! and passes the result in `RankingWeights::recency_weight`.

use super::ScoreRefiner;
use crate::retriever::RankingWeights;
use crate::storage::HunkHit;
use async_trait::async_trait;

pub struct RecencyRefiner {
    weights: RankingWeights,
    now_unix: i64,
}

impl RecencyRefiner {
    /// Construct with weights and the current Unix timestamp.
    /// The coordinator passes `now_unix` from the outer call so all
    /// hits in one pipeline run are ranked against the same instant.
    pub fn new(weights: RankingWeights, now_unix: i64) -> Self {
        Self { weights, now_unix }
    }
}

#[async_trait]
impl ScoreRefiner for RecencyRefiner {
    async fn refine(
        &self,
        _query_text: &str,
        hits: Vec<HunkHit>,
    ) -> crate::Result<Vec<HunkHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        let mut scored: Vec<(HunkHit, f32)> = hits
            .into_iter()
            .map(|h| {
                let age_days =
                    ((self.now_unix - h.commit.ts).max(0) as f32) / 86_400.0;
                let recency =
                    (-age_days / self.weights.recency_half_life_days).exp();
                let combined = h.similarity
                    * (1.0 + self.weights.recency_weight * recency);
                (h, combined)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(scored.into_iter().map(|(h, _)| h).collect())
    }
}
```

- [ ] **Step 4: Run the tests**

```
cargo test -p ohara-core -- recency_refiner
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/refiners/recency.rs
git commit -m "test(core): plan-20 RecencyRefiner contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/refiners/recency.rs
git commit -m "feat(core): plan-20 RecencyRefiner — half-life recency refiner"
```

---

## Phase D — Coordinator

Replace `find_pattern_with_profile`'s body with the 5-step coordinator.
Lanes and refiners are constructed once at `Retriever::new` / builder time.
The existing public API stays.

### Task D.1 — `coordinator.rs`

**Files:**
- Create: `crates/ohara-core/src/retriever/coordinator.rs`
- Modify: `crates/ohara-core/src/retriever/mod.rs`

- [ ] **Step 1: Write the failing test**

The coordinator test re-creates the `find_pattern_invokes_three_lanes_and_rrf`
scenario using the coordinator's `run` function directly with fake lanes and
refiners:

Append to `crates/ohara-core/src/retriever/coordinator.rs` (to be created):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{PatternQuery};
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use crate::retriever::{RetrievalLane, LaneId, ScoreRefiner};
    use async_trait::async_trait;

    fn make_hit(id: HunkId, sim: f32) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "x".into(), file_path: "f.rs".into(), language: None, change_kind: ChangeKind::Added, diff_text: format!("diff-{id}") },
            commit: CommitMeta { commit_sha: "x".into(), parent_sha: None, is_merge: false, author: None, ts: 1_700_000_000, message: "m".into() },
            similarity: sim,
        }
    }

    struct StaticLane(LaneId, Vec<HunkHit>);

    #[async_trait]
    impl RetrievalLane for StaticLane {
        fn id(&self) -> LaneId { self.0 }
        async fn search(&self, _: &PatternQuery, _: &RepoId, _: usize) -> crate::Result<Vec<HunkHit>> {
            Ok(self.1.clone())
        }
    }

    struct IdentityRefiner;

    #[async_trait]
    impl ScoreRefiner for IdentityRefiner {
        async fn refine(&self, _: &str, hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn coordinator_rrf_merges_lanes() {
        // Three lanes return overlapping ids.
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(LaneId::Vec, vec![make_hit(1, 0.9), make_hit(2, 0.5)])),
            Box::new(StaticLane(LaneId::Bm25Text, vec![make_hit(2, 0.8), make_hit(1, 0.3)])),
            Box::new(StaticLane(LaneId::Bm25HistSym, vec![make_hit(3, 0.4)])),
        ];
        let refiners: Vec<Box<dyn ScoreRefiner>> = vec![Box::new(IdentityRefiner)];
        let q = PatternQuery { query: "test".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &refiners, &q, &repo_id, 10, 20).await.unwrap();
        assert_eq!(out.len(), 3, "all three unique ids survive rrf");
        // ids 1 and 2 appear in two lanes each; id=3 in one.
        // RRF places ids 1 and 2 above id 3.
        assert!(out.iter().position(|h| h.hunk_id == 3).unwrap() > 0,
            "id=3 (single-lane) must rank below the two-lane ids");
    }

    #[tokio::test]
    async fn coordinator_applies_refiners_in_sequence() {
        // Refiner that reverses the list (to confirm ordering is refiner-driven).
        struct ReverseRefiner;
        #[async_trait]
        impl ScoreRefiner for ReverseRefiner {
            async fn refine(&self, _: &str, mut hits: Vec<HunkHit>) -> crate::Result<Vec<HunkHit>> {
                hits.reverse();
                Ok(hits)
            }
        }
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(StaticLane(LaneId::Vec, vec![make_hit(10, 0.9), make_hit(11, 0.5)])),
        ];
        let refiners: Vec<Box<dyn ScoreRefiner>> = vec![Box::new(ReverseRefiner)];
        let q = PatternQuery { query: "anything".into(), k: 5, language: None, since_unix: None, no_rerank: false };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let out = run(&lanes, &refiners, &q, &repo_id, 10, 20).await.unwrap();
        // RRF with single lane: id=10 first (rank 1). Reverse refiner flips → id=11 first.
        assert_eq!(out[0].hunk_id, 11);
        assert_eq!(out[1].hunk_id, 10);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```
cargo test -p ohara-core coordinator_rrf_merges_lanes -- --nocapture
```

Expected: FAIL.

- [ ] **Step 3: Implement `coordinator.rs`**

Create `crates/ohara-core/src/retriever/coordinator.rs`:

```rust
//! Plan 20 — retrieval coordinator.
//!
//! Wires the 5-step pipeline:
//! 1. Fire all lanes in parallel via `join_all`.
//! 2. RRF-merge into one ranked list (free function — not a trait).
//! 3. Truncate to `rerank_pool_k` before the expensive refiners.
//! 4. Apply each `ScoreRefiner` in sequence.
//! 5. Truncate to caller's `k`.

use crate::query::{reciprocal_rank_fusion, PatternQuery};
use crate::retriever::{LaneId, RetrievalLane, ScoreRefiner};
use crate::storage::{HunkHit, HunkId};
use crate::types::RepoId;
use futures::future::join_all;
use std::collections::HashMap;

/// Run the full coordinator pipeline.
///
/// - `lanes`: all lane instances (disabled lanes self-skip by returning empty).
/// - `refiners`: applied in order to the post-RRF candidate list.
/// - `rerank_pool_k`: how many post-RRF candidates to feed into refiners.
/// - `final_k`: hard truncation after refiners.
pub async fn run(
    lanes: &[Box<dyn RetrievalLane>],
    refiners: &[Box<dyn ScoreRefiner>],
    query: &PatternQuery,
    repo_id: &RepoId,
    rerank_pool_k: usize,
    final_k: usize,
) -> crate::Result<Vec<HunkHit>> {
    // 1. Fire all lanes in parallel. Disabled lanes return Ok(vec![])
    //    without touching storage.
    let lane_futures = lanes.iter().map(|l| l.search(query, repo_id, final_k));
    let lane_results: Vec<crate::Result<Vec<HunkHit>>> = join_all(lane_futures).await;

    // 2. Build per-lane ranked id lists + a HunkId -> HunkHit lookup.
    let mut by_id: HashMap<HunkId, HunkHit> = HashMap::new();
    let mut rankings: Vec<Vec<HunkId>> = Vec::with_capacity(lanes.len());
    for result in lane_results {
        let hits = result?;
        let ranking: Vec<HunkId> = hits
            .iter()
            .map(|h| {
                by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
                h.hunk_id
            })
            .collect();
        rankings.push(ranking);
    }

    // 3. RRF merge (k=60, Cormack 2009) → truncate to rerank pool.
    let fused: Vec<HunkId> = reciprocal_rank_fusion(&rankings, 60);
    let pool: Vec<HunkHit> = fused
        .into_iter()
        .take(rerank_pool_k)
        .filter_map(|id| by_id.get(&id).cloned())
        .collect();

    if pool.is_empty() {
        return Ok(vec![]);
    }

    // 4. Apply refiners in sequence.
    let mut hits = pool;
    for refiner in refiners {
        hits = refiner.refine(&query.query, hits).await?;
    }

    // 5. Truncate to final k.
    hits.truncate(final_k);
    Ok(hits)
}
```

- [ ] **Step 4: Add `futures` dependency**

In the root `Cargo.toml` `[workspace.dependencies]` (if not present):

```toml
futures = "0.3"
```

In `crates/ohara-core/Cargo.toml` `[dependencies]`:

```toml
futures.workspace = true
```

- [ ] **Step 5: Add `pub mod coordinator;` to `retriever/mod.rs`**

```rust
pub mod coordinator;
```

- [ ] **Step 6: Run the coordinator tests**

```
cargo test -p ohara-core -- coordinator
```

Expected: both coordinator tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/retriever/coordinator.rs Cargo.toml crates/ohara-core/Cargo.toml
git commit -m "test(core): plan-20 coordinator pipeline contract (failing)"
```

```bash
git add crates/ohara-core/src/retriever/coordinator.rs Cargo.toml crates/ohara-core/Cargo.toml
git commit -m "feat(core): plan-20 coordinator — join_all lanes + RRF + sequential refiners"
```

---

### Task D.2 — Wire coordinator into `Retriever`

Replace `find_pattern_with_profile`'s body. Lanes and refiners are
constructed once at `Retriever::new` / builder time and stored in the
struct.

**Files:**
- Modify: `crates/ohara-core/src/retriever/mod.rs`

- [ ] **Step 1: Extend the `Retriever` struct**

Replace the current `Retriever` definition:

```rust
pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
    reranker: Option<Arc<dyn RerankProvider>>,
}
```

with:

```rust
pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
    reranker: Option<Arc<dyn RerankProvider>>,
    // Plan 20: pre-built lane and refiner lists.
    // Lanes are constructed lazily in `find_pattern_with_profile`
    // because they hold Arc refs — construction is cheap.
    // We keep the Option<reranker> field for the `with_reranker`
    // builder API compatibility; Phase E may collapse this further.
}
```

The lane and refiner lists are constructed inside
`find_pattern_with_profile` (they hold `Arc` refs — construction is O(1)
and the struct stays `Clone`-able). If future profiling shows construction
overhead, a follow-up plan can cache them.

- [ ] **Step 2: Replace `find_pattern_with_profile` body**

Replace the entire body of `find_pattern_with_profile` with:

```rust
    pub async fn find_pattern_with_profile(
        &self,
        repo_id: &crate::types::RepoId,
        query: &PatternQuery,
        now_unix: i64,
    ) -> crate::Result<(
        Vec<PatternHit>,
        crate::query_understanding::RetrievalProfile,
    )> {
        use crate::retriever::coordinator;
        use crate::retriever::lanes::{
            bm25_head_sym::Bm25HeadSymLane,
            bm25_hist_sym::Bm25HistSymLane,
            bm25_text::Bm25TextLane,
            vec::VecLane,
            RetrievalLane,
        };
        use crate::retriever::refiners::{
            cross_encoder::CrossEncoderRefiner,
            recency::RecencyRefiner,
            ScoreRefiner,
        };

        let parsed = crate::query_understanding::parse_query(&query.query);
        let profile = crate::query_understanding::RetrievalProfile::for_intent(parsed.intent);

        let rerank_top_k = profile.rerank_top_k.unwrap_or(self.weights.rerank_top_k);

        // Build lanes (profile-gating is inside each lane via is_lane_enabled).
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(VecLane::new(self.storage.clone(), self.embedder.clone())),
            Box::new(Bm25TextLane::new(self.storage.clone())),
            Box::new(Bm25HistSymLane::new(self.storage.clone())),
            Box::new(Bm25HeadSymLane::new(self.storage.clone())),
        ];

        // Build refiners. The effective recency weight folds in the
        // profile's multiplier so the RecencyRefiner is self-contained.
        let effective_recency_weight =
            self.weights.recency_weight * profile.recency_multiplier;
        let mut effective_weights = self.weights.clone();
        effective_weights.recency_weight = effective_recency_weight;

        let mut refiners: Vec<Box<dyn ScoreRefiner>> = Vec::new();
        if let Some(reranker) = &self.reranker {
            if !query.no_rerank {
                refiners.push(Box::new(CrossEncoderRefiner::new(reranker.clone())));
            }
        }
        refiners.push(Box::new(RecencyRefiner::new(effective_weights, now_unix)));

        // Run coordinator.
        let final_k = query.k.clamp(1, 20) as usize;
        let raw_hits = coordinator::run(
            &lanes,
            &refiners,
            query,
            repo_id,
            rerank_top_k,
            final_k,
        )
        .await?;

        if raw_hits.is_empty() {
            return Ok((vec![], profile));
        }

        // Hydrate per-hunk symbol attribution rows (unchanged from pre-plan-20).
        let symbols_by_hunk: std::collections::HashMap<HunkId, Vec<String>> = {
            let mut acc: std::collections::HashMap<HunkId, Vec<String>> =
                std::collections::HashMap::new();
            for h in &raw_hits {
                let attrs = self.storage.get_hunk_symbols(repo_id, h.hunk_id).await?;
                if !attrs.is_empty() {
                    acc.insert(h.hunk_id, attrs.into_iter().map(|a| a.name).collect());
                }
            }
            acc
        };

        // Map HunkHit → PatternHit (unchanged from pre-plan-20).
        let out: Vec<PatternHit> = raw_hits
            .into_iter()
            .map(|h| {
                let date = chrono::DateTime::<chrono::Utc>::from_timestamp(h.commit.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                let (excerpt, truncated) =
                    crate::diff_text::truncate_diff(&h.hunk.diff_text, crate::diff_text::DIFF_EXCERPT_MAX_LINES);
                let related_head_symbols =
                    symbols_by_hunk.get(&h.hunk_id).cloned().unwrap_or_default();
                crate::query::PatternHit {
                    commit_sha: h.commit.commit_sha,
                    commit_message: h.commit.message,
                    commit_author: h.commit.author,
                    commit_date: date,
                    file_path: h.hunk.file_path,
                    change_kind: format!("{:?}", h.hunk.change_kind).to_lowercase(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    related_head_symbols,
                    similarity: h.similarity,
                    recency_weight: {
                        let age = ((now_unix - h.commit.ts).max(0) as f32) / 86_400.0;
                        (-age / self.weights.recency_half_life_days).exp()
                    },
                    combined_score: h.similarity, // coordinator already applied recency
                    provenance: crate::types::Provenance::Inferred,
                }
            })
            .collect();

        Ok((out, profile))
    }
```

- [ ] **Step 3: Remove now-unused imports from `retriever/mod.rs`**

After the substitution, `chrono`, `RerankProvider` (direct use),
`timed_phase`, and `HashMap` may still be needed for the hydration block
or PatternHit mapping. Audit with `cargo clippy` and remove any unused
`use` items.

- [ ] **Step 4: Run the full test suite**

```
cargo test -p ohara-core
```

Expected: all existing retriever tests pass (the fake `FakeStorage` in
`retriever/mod.rs` still satisfies the `Storage` trait; the coordinator
calls the same storage methods through the lane impls).

```
cargo test --workspace
```

Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/mod.rs
git commit -m "refactor(core): plan-20 wire coordinator into Retriever::find_pattern_with_profile"
```

---

## Phase E — Cleanup

### Task E.1 — Delete inline helpers and verify line count

**Files:**
- Modify: `crates/ohara-core/src/retriever/mod.rs`

After Phase D, `retriever/mod.rs` should no longer contain the original
inline join/RRF/rerank/recency code. Confirm with:

```bash
wc -l crates/ohara-core/src/retriever/mod.rs
```

Target: under 300 lines.

- [ ] **Step 1: Confirm no dead inline code remains**

Run:

```
cargo clippy -p ohara-core -- -D warnings
```

Any unused function or import surfaced by clippy must be removed.

- [ ] **Step 2: Run the full workspace**

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: all clean and green.

- [ ] **Step 3: Verify the perf harness still compiles**

```
cargo build --release -p ohara-perf-tests 2>/dev/null || cargo build -p ohara-perf-tests
```

Expected: compiles cleanly (perf harness does not call retriever internals
directly).

- [ ] **Step 4: Update `CLAUDE.md` workspace layout section**

In `CLAUDE.md`, the architecture cheatsheet entry for `ohara-core` says:

> **Retrieval pipeline** (`ohara-core::retriever`): three lanes — vector
> KNN, BM25 over hunk text, BM25 over symbol names — fused via Reciprocal
> Rank Fusion → cross-encoder reranked …

Update to:

> **Retrieval pipeline** (`ohara-core::retriever`): four lanes
> (`VecLane`, `Bm25TextLane`, `Bm25HistSymLane`, `Bm25HeadSymLane`) each
> implementing `RetrievalLane`, fired in parallel via `join_all` in
> `coordinator::run`; RRF-merged; two `ScoreRefiner` impls
> (`CrossEncoderRefiner`, `RecencyRefiner`) applied in sequence. Profile
> lane-gating lives inside each lane via `is_lane_enabled`. `RankingWeights`
> remains the tuning surface for recency parameters.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/retriever/ CLAUDE.md
git commit -m "refactor(core): plan-20 cleanup — remove inline helpers, update CLAUDE.md"
```

---

## Pre-completion checklist (CONTRIBUTING.md §13)

Run all of the following before opening a PR for plan-20:

- [ ] `cargo fmt --all` — clean (no diff)
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` — zero
  warnings
- [ ] `cargo test --workspace` — all tests green
- [ ] `wc -l crates/ohara-core/src/retriever/mod.rs` — under 300 lines
- [ ] No `unwrap()` / `expect()` without the `"invariant: <reason>"` form in
  non-test code
- [ ] No `println!` / `eprintln!` outside `ohara-cli` user-facing output
- [ ] No new top-level `*.md` files
- [ ] `ohara-core` has no new direct dependency on `ohara-storage` /
  `ohara-embed` / `ohara-git` / `ohara-parse` (check `Cargo.toml`)
- [ ] Each lane constructor takes an `Arc<dyn …>` provider, not a concrete type
- [ ] `RRF` merging remains a free function in `coordinator.rs` (not promoted
  to a trait)
- [ ] Existing retriever tests in `retriever/mod.rs` pass without modification
- [ ] Performance sanity: run `cargo bench` or the perf harness to confirm
  query latency is not regressed relative to the pre-plan-20 baseline
  (the `join_all` path is semantically equivalent to `tokio::join!` for
  a fixed-size lane set; verify the diff in the harness output is <5%)

---

## Risks

**1. `join_all` vs. `tokio::join!` parallelism.**
The current body uses `tokio::join!(…, …, …, …)` which is a macro that
generates a single `tokio` future driving four branches simultaneously.
`join_all` over a `Vec<Future>` is semantically equivalent for a fixed
set of known-size futures, but it goes through a heap-allocated `Vec`
and a polling loop rather than inlined state machine code. In practice
the network/storage round-trip time dominates for SQLite (microseconds
to milliseconds), making the overhead negligible. Verify with the
`cli_query_bench` perf harness before and after Phase D. Target: p95
latency within 5% of baseline.

**2. Semaphore policy for `CrossEncoderRefiner`.**
The daemon (plan-16) holds a `Semaphore(1)` around the full
`find_pattern_with_profile` call to serialise model inference. The
`CrossEncoderRefiner::refine` method makes a plain `reranker.rerank()`
call without internal synchronisation. This is the correct layering:
the daemon's lock site wraps the whole pipeline, not the refiner. If
the refiner is ever called concurrently from multiple pipeline
invocations (e.g., a future multi-tenant daemon mode), the semaphore
must be moved to the refiner or to a pool. Document this in the refiner
module's doc comment (done in Task C.1 Step 3).

**3. `profile.vec_lane_enabled` / `text_lane_enabled` / `symbol_lane_enabled`
field renaming.**
The `is_lane_enabled(LaneId)` method introduced in Task A.1 is the new
stable API; the three struct fields remain for backward compatibility
with any external code that reads them (e.g., the MCP response metadata
serialises the full `RetrievalProfile`). No rename is needed in this
plan. A future plan may add `#[serde(skip)]` to the raw flag fields
and expose only `is_lane_enabled` as the programmatic gate.

**4. Coordinator `PatternHit` `combined_score` field.**
In the pre-plan-20 code the `combined_score` on `PatternHit` is the
reranker score multiplied by the recency factor. After plan-20, the
`RecencyRefiner` reorders hits but the `combined_score` field on the
output `PatternHit` is set to `h.similarity` (the lane-level score)
because the coordinator does not thread scores back through the `HunkHit`
struct. This changes the semantics of `combined_score` in the MCP
`_meta` output: it will now always equal `similarity`. A follow-up plan
should either add a `combined_score` field to `HunkHit` or accept that
`combined_score == similarity` is the new contract. Surface this
decision in the PR description.
