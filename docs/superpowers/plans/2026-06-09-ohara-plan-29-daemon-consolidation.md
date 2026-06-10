# Plan 29: Daemon Consolidation (single shared engine host) + Plugin Auto-Version

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** N concurrent MCP sessions share exactly one engine-hosting daemon process (tiered model unload), and the Claude Code plugin stops pinning a stale binary version.

**Architecture:** `ohara-mcp` becomes a thin IPC client of the plan-16 daemon (spawning *itself* as the daemon via a new `serve` mode, since plugin installs don't ship the CLI). The serve runner moves into `ohara-engine` so both binaries share it. Daemon find-or-spawn becomes atomic under the registry file lock; old-version daemons are swept at startup; the reranker session unloads after idle via a reusable `IdleSlot`. The plugin wrapper reads its version from `plugin.json`, enforced by a CI drift check.

**Tech Stack:** Rust (tokio, clap, fs2, rmcp), node:test for the plugin wrapper.

**Specs:** `docs/superpowers/specs/2026-06-09-ohara-daemon-consolidation-design.md` (A1–A3), `docs/superpowers/specs/2026-06-09-ohara-usability-plan-design.md` (B1).

**Verified context (read before deviating):**
- `ohara-engine` already depends on `ohara-embed` — the daemon runner CAN construct providers inside `ohara-engine`.
- IPC already has `Ping/Shutdown/FindPattern/ExplainChange/InvalidateRepo/IndexStatus/Metrics`; `IndexStatus` is a `NotImplemented` stub (`crates/ohara-engine/src/server.rs`).
- Nothing in production sets `DaemonRecord.busy = true`; the daemon serves each connection in its own task.
- The `Indexer` only stamps index metadata when `.with_runtime_metadata(...)` is set. Engine tests don't set it → stored metadata is empty → `CompatibilityStatus::assess` returns `Unknown`, NOT `NeedsRebuild`. The new engine-side guard therefore does not break existing engine tests.
- `tokio::sync::OnceCell` is already a dependency surface (`LazyFastEmbedReranker`).
- Conventions that apply to every task: no `else` (guard clauses / `match` / let-else), no `unwrap()/expect()` outside tests (`expect("invariant: ...")` allowed), files < 500 lines, library errors via `thiserror`, all deps via `[workspace.dependencies]`. Run `cargo fmt --all` before every commit.

---

### Task 1: `IdleSlot<T>` — reloadable lazy cell with idle unload (ohara-embed)

The reranker currently lives in a `tokio::sync::OnceCell` (load-once, never unloads). `IdleSlot` is the replacement primitive: lazy init, last-used tracking, idle-based drop, transparent reload. Generic so it is testable without loading a 110 MB model.

**Files:**
- Create: `crates/ohara-embed/src/idle_slot.rs`
- Modify: `crates/ohara-embed/src/lib.rs` (add `mod idle_slot;`)

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-embed/src/idle_slot.rs` with the tests first (module body referenced by them comes in Step 3):

```rust
//! Reloadable lazy slot with idle-based unload.
//!
//! Backs `LazyFastEmbedReranker` (and any future lazily-loaded model
//! session): the value is created on first use, its last-used time is
//! tracked, and `unload_if_idle` drops it after a quiet period so the
//! next use transparently reloads it.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn initialises_once_and_reuses_value() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let inits = AtomicUsize::new(0);
        let v1: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("model".to_string())
            })
            .await;
        let v2: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("model".to_string())
            })
            .await;
        assert_eq!(v1.unwrap(), "model");
        assert_eq!(v2.unwrap(), "model");
        assert_eq!(inits.load(Ordering::SeqCst), 1, "second call must not re-init");
    }

    #[tokio::test]
    async fn failed_init_is_retried() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let r1: Result<String, String> = slot.get_or_try_init(async { Err("boom".to_string()) }).await;
        assert!(r1.is_err());
        let r2: Result<String, String> = slot
            .get_or_try_init(async { Ok("recovered".to_string()) })
            .await;
        assert_eq!(r2.unwrap(), "recovered");
    }

    #[tokio::test]
    async fn unload_on_empty_slot_is_false() {
        let slot: IdleSlot<String> = IdleSlot::new();
        assert!(!slot.unload_if_idle(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn unload_after_idle_drops_and_reload_reinits() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let _: Result<String, ()> = slot.get_or_try_init(async { Ok("v1".to_string()) }).await;
        // Fresh value: a 1-hour idle threshold must NOT unload it.
        assert!(!slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Force the last-used stamp into the past, then unload.
        slot.force_last_used(1);
        assert!(slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Unloading twice is false (already empty).
        slot.force_last_used(1);
        assert!(!slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Next get re-initialises.
        let inits = AtomicUsize::new(0);
        let v: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("v2".to_string())
            })
            .await;
        assert_eq!(v.unwrap(), "v2");
        assert_eq!(inits.load(Ordering::SeqCst), 1);
    }
}
```

Add `mod idle_slot;` to `crates/ohara-embed/src/lib.rs` (below the existing `mod fastembed;`-style declarations).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ohara-embed idle_slot`
Expected: COMPILE ERROR — `IdleSlot` not found.

- [ ] **Step 3: Commit the red test**

```bash
git add crates/ohara-embed/src/idle_slot.rs crates/ohara-embed/src/lib.rs
git commit -m "test(embed): IdleSlot lazy-init/idle-unload contract (red)"
```

- [ ] **Step 4: Write the implementation** (above the `tests` module in `idle_slot.rs`):

```rust
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

pub(crate) struct IdleSlot<T> {
    slot: RwLock<Option<T>>,
    /// Unix-seconds of the most recent `get_or_try_init` call; 0 = never used.
    last_used_unix: AtomicU64,
}

impl<T: Clone> IdleSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            slot: RwLock::new(None),
            last_used_unix: AtomicU64::new(0),
        }
    }

    /// Return the value, initialising via `init` when the slot is empty.
    /// Refreshes the last-used stamp. Failed inits leave the slot empty,
    /// so the next call retries.
    pub(crate) async fn get_or_try_init<E>(
        &self,
        init: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        self.last_used_unix.store(now_unix(), Ordering::Relaxed);
        if let Some(v) = self.slot.read().await.clone() {
            return Ok(v);
        }
        let mut guard = self.slot.write().await;
        // Double-check: another task may have initialised while we
        // waited for the write lock.
        if let Some(v) = guard.clone() {
            return Ok(v);
        }
        let v = init.await?;
        *guard = Some(v.clone());
        Ok(v)
    }

    /// Drop the value when it has been idle for at least `idle`.
    /// Returns `true` when a value was dropped.
    pub(crate) async fn unload_if_idle(&self, idle: Duration) -> bool {
        let last = self.last_used_unix.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        if now_unix().saturating_sub(last) < idle.as_secs() {
            return false;
        }
        let mut guard = self.slot.write().await;
        if guard.is_none() {
            return false;
        }
        *guard = None;
        true
    }

    #[cfg(test)]
    pub(crate) fn force_last_used(&self, unix: u64) {
        self.last_used_unix.store(unix, Ordering::Relaxed);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ohara-embed idle_slot`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/ohara-embed/src/idle_slot.rs
git commit -m "feat(embed): IdleSlot — reloadable lazy cell with idle unload"
```

---

### Task 2: `RerankProvider::unload_if_idle` + `LazyFastEmbedReranker` on `IdleSlot`

**Files:**
- Modify: `crates/ohara-core/src/embed.rs` (trait default method)
- Modify: `crates/ohara-embed/src/fastembed.rs` (`LazyFastEmbedReranker`, ~lines 273–350)

- [ ] **Step 1: Add the trait method with default impl** in `crates/ohara-core/src/embed.rs`, inside `pub trait RerankProvider` (after `rerank`):

```rust
    /// Drop any lazily-loaded model session that has been idle for at
    /// least `idle`. Returns `true` when a session was dropped.
    ///
    /// Default is a no-op `false`: eager rerankers and test stubs hold
    /// no unloadable state. `LazyFastEmbedReranker` overrides this so
    /// the `ohara serve` daemon can shed the ~110 MB cross-encoder
    /// session during quiet periods (plan-29 tiered unload).
    async fn unload_if_idle(&self, _idle: std::time::Duration) -> bool {
        false
    }
```

- [ ] **Step 2: Write the failing tests** in the existing `#[cfg(test)] mod tests` of `crates/ohara-embed/src/fastembed.rs`:

```rust
    #[tokio::test]
    async fn lazy_reranker_unload_without_load_is_false() {
        use ohara_core::embed::RerankProvider as _;
        let r = LazyFastEmbedReranker::new();
        assert!(
            !r.unload_if_idle(std::time::Duration::ZERO).await,
            "nothing loaded yet — nothing to unload"
        );
    }

    #[tokio::test]
    async fn lazy_reranker_empty_candidates_never_loads_or_stamps() {
        use ohara_core::embed::RerankProvider as _;
        let r = LazyFastEmbedReranker::new();
        let scores = r.rerank("q", &[]).await.expect("empty rerank");
        assert!(scores.is_empty());
        // Still nothing to unload: the empty-candidates short-circuit
        // must not have touched the slot.
        assert!(!r.unload_if_idle(std::time::Duration::ZERO).await);
    }
```

- [ ] **Step 3: Run to verify the second test fails to compile or fails**

Run: `cargo test -p ohara-embed lazy_reranker`
Expected: compile error (`unload_if_idle` exists via default trait method, returns false — first test passes; if both pass trivially, proceed: these tests pin the contract for the rework). Commit either way after seeing the run:

```bash
git add crates/ohara-core/src/embed.rs crates/ohara-embed/src/fastembed.rs
git commit -m "test(embed): pin LazyFastEmbedReranker unload contract (red)"
```

- [ ] **Step 4: Rework `LazyFastEmbedReranker`** in `crates/ohara-embed/src/fastembed.rs`. Replace the struct/impl (keep the existing doc comment, extend it with one line about plan-29 unload):

```rust
pub struct LazyFastEmbedReranker {
    slot: crate::idle_slot::IdleSlot<Arc<FastEmbedReranker>>,
    provider: EmbedProvider,
}

impl LazyFastEmbedReranker {
    pub fn new() -> Self {
        Self::with_provider(EmbedProvider::Cpu)
    }

    pub fn with_provider(provider: EmbedProvider) -> Self {
        Self {
            slot: crate::idle_slot::IdleSlot::new(),
            provider,
        }
    }

    pub fn model_id(&self) -> &'static str {
        DEFAULT_RERANKER_ID
    }

    async fn get_or_init(&self) -> CoreResult<Arc<FastEmbedReranker>> {
        let provider = self.provider;
        self.slot
            .get_or_try_init(async move {
                tokio::task::spawn_blocking(move || FastEmbedReranker::with_provider(provider))
                    .await
                    .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?
                    .map(Arc::new)
                    .map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
            })
            .await
    }
}
```

And the trait impl:

```rust
#[async_trait::async_trait]
impl RerankProvider for LazyFastEmbedReranker {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> CoreResult<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        self.get_or_init().await?.rerank(query, candidates).await
    }

    async fn unload_if_idle(&self, idle: std::time::Duration) -> bool {
        self.slot.unload_if_idle(idle).await
    }
}
```

Remove the now-unused `OnceCell` import if nothing else in the file uses it. `Arc` is `std::sync::Arc` (already imported). Note `IdleSlot` is `pub(crate)` — same crate, fine.

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p ohara-embed`
Expected: all pass (including pre-existing lazy-reranker tests — public API unchanged).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/ohara-core/src/embed.rs crates/ohara-embed/src/fastembed.rs
git commit -m "feat(embed): reranker idle unload via IdleSlot (RerankProvider::unload_if_idle)"
```

---

### Task 3: `LazyFastEmbedProvider` — embedder that loads on first embed

Used by the `ohara-mcp` in-process fallback so a session that never queries (or only calls `explain_change`) never loads a model.

**Files:**
- Modify: `crates/ohara-embed/src/fastembed.rs` (new struct after `LazyFastEmbedReranker`)
- Modify: `crates/ohara-embed/src/lib.rs` (re-export)

- [ ] **Step 1: Write the failing test** (in `fastembed.rs` tests module):

```rust
    #[test]
    fn lazy_embedder_reports_identity_without_init() {
        use ohara_core::EmbeddingProvider as _;
        let e = LazyFastEmbedProvider::new();
        // Both must answer from constants — constructing the provider
        // and asking for identity must never load the ONNX session.
        assert_eq!(e.model_id(), DEFAULT_MODEL_ID);
        assert_eq!(e.dimension(), DEFAULT_DIM);
    }
```

Run: `cargo test -p ohara-embed lazy_embedder` → COMPILE ERROR. Commit:

```bash
git add crates/ohara-embed/src/fastembed.rs
git commit -m "test(embed): lazy embedder identity without init (red)"
```

- [ ] **Step 2: Implement**

```rust
/// Lazy wrapper around [`FastEmbedProvider`]: defers loading the
/// BGE-small ONNX session until the first `embed_batch` call.
///
/// Plan-29: the `ohara-mcp` in-process fallback uses this so MCP
/// sessions that never call `find_pattern` (or only call
/// `explain_change`, which needs no models) never pay the embedder
/// load. Identity methods answer from compile-time constants.
pub struct LazyFastEmbedProvider {
    slot: crate::idle_slot::IdleSlot<Arc<FastEmbedProvider>>,
    provider: EmbedProvider,
}

impl LazyFastEmbedProvider {
    pub fn new() -> Self {
        Self::with_provider(EmbedProvider::Cpu)
    }

    pub fn with_provider(provider: EmbedProvider) -> Self {
        Self {
            slot: crate::idle_slot::IdleSlot::new(),
            provider,
        }
    }

    async fn get_or_init(&self) -> CoreResult<Arc<FastEmbedProvider>> {
        let provider = self.provider;
        self.slot
            .get_or_try_init(async move {
                tokio::task::spawn_blocking(move || FastEmbedProvider::with_provider(provider))
                    .await
                    .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?
                    .map(Arc::new)
                    .map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
            })
            .await
    }
}

impl Default for LazyFastEmbedProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ohara_core::EmbeddingProvider for LazyFastEmbedProvider {
    fn dimension(&self) -> usize {
        DEFAULT_DIM
    }

    fn model_id(&self) -> &str {
        DEFAULT_MODEL_ID
    }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        self.get_or_init().await?.embed_batch(texts).await
    }
}
```

In `crates/ohara-embed/src/lib.rs`, add `LazyFastEmbedProvider` to the existing `pub use` list (line ~6, next to `LazyFastEmbedReranker`).

- [ ] **Step 3: Run, then commit**

Run: `cargo test -p ohara-embed` → all pass.

```bash
cargo fmt --all
git add crates/ohara-embed/src/fastembed.rs crates/ohara-embed/src/lib.rs
git commit -m "feat(embed): LazyFastEmbedProvider — embedder loads on first embed_batch"
```

---

### Task 4: Engine — NeedsRebuild guard, `index_status`, `unload_idle_reranker`, IPC `IndexStatus`

Moves the find_pattern compatibility refusal from the MCP tool into the engine (one guard for daemon, CLI, and MCP fallback paths), implements the `IndexStatus` IPC stub, and exposes reranker unload for the watchdog.

**Files:**
- Modify: `crates/ohara-engine/src/engine.rs`
- Modify: `crates/ohara-engine/src/server.rs` (dispatch `IndexStatus`)
- Modify: `crates/ohara-engine/src/engine_tests.rs`

- [ ] **Step 1: Write the failing tests** (append to `engine_tests.rs`):

```rust
// env_lock held across awaits intentionally — see note above the
// explain_change test.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn find_pattern_refuses_when_index_needs_rebuild() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let canonical = std::fs::canonicalize(tmp.path()).unwrap();

    // Index, then sabotage the vector-affecting metadata so
    // CompatibilityStatus::assess yields NeedsRebuild.
    {
        let walker = ohara_git::GitWalker::open(&canonical).unwrap();
        let first = walker.first_commit_sha().unwrap();
        let repo_id =
            ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
        let storage: Arc<dyn ohara_core::Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
        let commit_src = Arc::new(ohara_git::GitCommitSource::open(&canonical).unwrap());
        let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&canonical).unwrap());
        let indexer = ohara_core::Indexer::new(storage.clone(), Arc::new(DummyEmbedder));
        indexer.run(&repo_id, commit_src, symbol_src).await.unwrap();
        storage
            .put_index_metadata(
                &repo_id,
                &[("embedding_model".to_string(), "other-model".to_string())],
            )
            .await
            .unwrap();
    }

    let engine = make_test_engine();
    let q = ohara_core::query::PatternQuery {
        query: "one".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let err = engine
        .find_pattern(&canonical, q)
        .await
        .expect_err("incompatible index must refuse");
    assert!(
        matches!(err, EngineError::NeedsRebuild { .. }),
        "expected NeedsRebuild, got: {err:?}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn index_status_returns_meta_for_indexed_repo() {
    let ohara_home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let _g = env_lock();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_test_repo(tmp.path());
    let engine = make_test_engine();
    let meta = engine.index_status(tmp.path()).await.expect("index_status");
    // Empty stored metadata → compatibility is present but not NeedsRebuild.
    let is_rebuild = matches!(
        meta.compatibility,
        Some(ohara_core::index_metadata::CompatibilityStatus::NeedsRebuild { .. })
    );
    assert!(!is_rebuild, "fresh test repo must not need rebuild");
}

#[tokio::test]
async fn unload_idle_reranker_with_dummy_is_false() {
    let engine = make_test_engine();
    // DummyReranker uses the trait default — nothing to unload.
    assert!(!engine.unload_idle_reranker(std::time::Duration::ZERO).await);
}
```

Run: `cargo test -p ohara-engine find_pattern_refuses` → COMPILE ERROR (`index_status`, `unload_idle_reranker` missing; guard absent). Commit:

```bash
git add crates/ohara-engine/src/engine_tests.rs
git commit -m "test(engine): NeedsRebuild guard, index_status, reranker unload (red)"
```

- [ ] **Step 2: Implement in `engine.rs`**

Extract the meta-cache block currently inlined in `find_pattern` (lines ~233–243) into a method on `RetrievalEngine`:

```rust
    async fn cached_meta(&self, handle: &RepoHandle) -> crate::Result<ResponseMeta> {
        if let Some(cached) = self.meta_cache.get(&handle.repo_id) {
            self.meta_hit_count.fetch_add(1, Ordering::Relaxed);
            return Ok(cached);
        }
        let fresh = compose_response_meta(handle).await?;
        self.meta_cache.put(handle.repo_id.clone(), fresh.clone());
        Ok(fresh)
    }
```

Rework `find_pattern` to guard before retrieval:

```rust
    pub async fn find_pattern(
        &self,
        repo_path: impl AsRef<Path>,
        query: PatternQuery,
    ) -> crate::Result<FindPatternResult> {
        let handle = self.open_repo(repo_path).await?;
        let meta = self.cached_meta(&handle).await?;
        // Plan-29: refuse before touching the vector index. KNN against
        // stale vectors silently returns wrong results; surfacing
        // NeedsRebuild here covers the daemon, CLI, and MCP-fallback
        // paths with one guard (previously the MCP tool checked locally).
        if let Some(CompatibilityStatus::NeedsRebuild { reason }) = &meta.compatibility {
            return Err(EngineError::NeedsRebuild {
                reason: reason.clone(),
            });
        }
        let now_unix = chrono::Utc::now().timestamp();
        let (hits, _profile) = handle
            .retriever
            .find_pattern_with_profile(&handle.repo_id, &query, now_unix)
            .await
            .map_err(EngineError::from)?;
        Ok(FindPatternResult { hits, meta })
    }
```

Add the two new public methods:

```rust
    /// Freshness + compatibility envelope for a repo, without running a
    /// query. Backs the IPC `IndexStatus` method so thin MCP clients can
    /// build the `_meta` block of `explain_change` responses.
    pub async fn index_status(&self, repo_path: impl AsRef<Path>) -> crate::Result<ResponseMeta> {
        let handle = self.open_repo(repo_path).await?;
        self.cached_meta(&handle).await
    }

    /// Ask the reranker to drop its session when idle for at least `idle`.
    /// Returns `true` when a session was dropped (plan-29 tiered unload).
    pub async fn unload_idle_reranker(&self, idle: std::time::Duration) -> bool {
        self.reranker.unload_if_idle(idle).await
    }
```

`CompatibilityStatus` is already imported in `engine.rs` (used by `compose_response_meta`); if it's imported in a function-local `use`, lift it to the module imports.

- [ ] **Step 3: Implement IPC dispatch** in `crates/ohara-engine/src/server.rs` — replace the `RequestMethod::IndexStatus => Err(EngineError::NotImplemented { ... })` arm:

```rust
        RequestMethod::IndexStatus => {
            let path = match req.repo_path {
                Some(p) => p,
                None => {
                    return error_response(id, ErrorCode::Internal, "index_status requires repo_path")
                }
            };
            match engine.index_status(&path).await {
                Ok(m) => serde_json::to_value(&m).map_err(|e| EngineError::Internal(e.to_string())),
                Err(e) => Err(e),
            }
        }
```

- [ ] **Step 4: Run**

Run: `cargo test -p ohara-engine`
Expected: all pass, including the pre-existing `find_pattern_meta_cached_within_ttl` (guard sees `Unknown`, not `NeedsRebuild`, on unstamped test indexes).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/ohara-engine/src/engine.rs crates/ohara-engine/src/server.rs
git commit -m "feat(engine): NeedsRebuild guard in find_pattern; IndexStatus IPC; reranker unload hook"
```

---

### Task 5: Move the serve runner into `ohara-engine::daemon` + reranker-unload watchdog

**Files:**
- Create: `crates/ohara-engine/src/daemon.rs`
- Modify: `crates/ohara-engine/src/lib.rs` (`pub mod daemon;`)
- Modify: `crates/ohara-cli/src/commands/serve.rs` (slims to arg-mapping)

- [ ] **Step 1: Write the failing test** in `daemon.rs` (file starts with tests + skeleton):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::tests::make_test_engine;
    use crate::ipc::{Request, RequestMethod};
    use std::sync::Arc;

    #[tokio::test]
    async fn run_daemon_with_engine_serves_ping_until_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = DaemonOptions {
            socket: tmp.path().join("d.sock"),
            pid_file: tmp.path().join("d.pid"),
            readiness_file: tmp.path().join("d.ready"),
            idle_timeout_secs: 0, // watchdog off for the test
            registry_path: None,
            reranker_idle_secs: 0,
        };
        let engine = Arc::new(make_test_engine());
        let socket = opts.socket.clone();
        let ready = opts.readiness_file.clone();
        let task = tokio::spawn(async move { run_daemon_with_engine(engine, opts).await });

        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ready.exists(), "daemon did not become ready");

        let ping = crate::client::Client::connect(&socket)
            .call(Request { id: 1, repo_path: None, method: RequestMethod::Ping })
            .await
            .expect("ping");
        assert!(ping.error.is_none());

        let _ = crate::client::Client::connect(&socket)
            .call(Request { id: 2, repo_path: None, method: RequestMethod::Shutdown })
            .await
            .expect("shutdown");
        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("daemon must exit after Shutdown")
            .expect("join");
        assert!(joined.is_ok(), "daemon exited with error: {joined:?}");
    }
}
```

Note: `crate::engine::tests::make_test_engine` is the same import the transport tests use. `Client` must be reachable as `crate::client::Client` — it is (`client/mod.rs` re-exports).

Run: `cargo test -p ohara-engine daemon` → COMPILE ERROR. Commit:

```bash
git add crates/ohara-engine/src/daemon.rs crates/ohara-engine/src/lib.rs
git commit -m "test(engine): daemon runner serves ping until shutdown (red)"
```

- [ ] **Step 2: Implement `daemon.rs`** — this is today's `crates/ohara-cli/src/commands/serve.rs::run` ported from `anyhow` to `EngineError`, parameterised on a pre-built engine, plus the new reranker watchdog:

```rust
//! Long-lived daemon runner shared by `ohara serve` and `ohara-mcp serve`.
//!
//! Owns the socket listener plus the watchdogs (readiness, registry
//! heartbeat, whole-process idle exit, reranker idle unload). Binaries
//! construct the engine (or call [`run_daemon`] for the default CPU
//! providers) — keeping provider choice in one place per binary.

use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::server::serve_unix;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct DaemonOptions {
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub readiness_file: PathBuf,
    /// Exit after this many seconds with no requests. 0 disables.
    pub idle_timeout_secs: u64,
    pub registry_path: Option<PathBuf>,
    /// Unload the lazily-loaded reranker session after this many seconds
    /// without a rerank. 0 disables (plan-29 tiered unload).
    pub reranker_idle_secs: u64,
}

/// Construct the default engine (CPU embedder, lazy reranker) and run.
pub async fn run_daemon(opts: DaemonOptions) -> crate::Result<()> {
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new)
            .await
            .map_err(|e| EngineError::Internal(format!("spawn_blocking embedder: {e}")))?
            .map_err(|e| EngineError::Embed(e.to_string()))?,
    );
    let reranker: Arc<dyn ohara_core::embed::RerankProvider> =
        Arc::new(ohara_embed::LazyFastEmbedReranker::new());
    let engine = Arc::new(RetrievalEngine::new(embedder, reranker));
    run_daemon_with_engine(engine, opts).await
}

/// Bind, write pid/readiness files, run watchdogs, serve until Shutdown
/// or idle exit. Testable with any engine.
pub async fn run_daemon_with_engine(
    engine: Arc<RetrievalEngine>,
    opts: DaemonOptions,
) -> crate::Result<()> {
    let stop = CancellationToken::new();
    let listener_engine = engine.clone();
    let listener_stop = stop.clone();
    let socket = opts.socket.clone();
    let mut listener =
        tokio::spawn(async move { serve_unix(listener_engine, &socket, listener_stop).await });

    // Surface a bind/startup error immediately rather than timing out.
    let ready = wait_for_socket(&opts.socket, Duration::from_secs(10));
    tokio::select! {
        biased;
        res = &mut listener => {
            return match res {
                Ok(Ok(())) => Err(EngineError::Internal("listener exited before socket was ready".into())),
                Ok(Err(e)) => Err(EngineError::Internal(format!("serve_unix failed at startup: {e}"))),
                Err(e) => Err(EngineError::Internal(format!("listener task join: {e}"))),
            }
        }
        res = ready => res?,
    }

    std::fs::write(&opts.pid_file, std::process::id().to_string())
        .map_err(|e| EngineError::Internal(format!("write pid file: {e}")))?;
    std::fs::write(&opts.readiness_file, "ready")
        .map_err(|e| EngineError::Internal(format!("write readiness file: {e}")))?;
    info!(socket = ?opts.socket, pid_file = ?opts.pid_file, "ohara daemon ready");

    if let Some(reg_path) = opts.registry_path.clone() {
        let pid = std::process::id();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if let Ok(reg) = crate::registry::Registry::open(&reg_path) {
                    let _ = reg.touch_health(pid);
                }
            }
        });
    }

    if opts.idle_timeout_secs > 0 {
        let idle = Duration::from_secs(opts.idle_timeout_secs);
        let watchdog_engine = engine.clone();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle / 2).await;
                if watchdog_engine.idle_for() >= idle {
                    info!(?idle, "idle timeout reached, shutting down");
                    watchdog_stop.cancel();
                    break;
                }
            }
        });
    }

    if opts.reranker_idle_secs > 0 {
        let idle = Duration::from_secs(opts.reranker_idle_secs);
        let watchdog_engine = engine.clone();
        let watchdog_stop = stop.clone();
        // Check at most once a minute; for small thresholds, at the
        // threshold itself (keeps tests fast and behavior predictable).
        let period = Duration::from_secs(opts.reranker_idle_secs.min(60).max(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if watchdog_engine.unload_idle_reranker(idle).await {
                    info!(idle_secs = idle.as_secs(), "reranker session unloaded after idle");
                }
            }
        });
    }

    let listener_result = listener
        .await
        .map_err(|e| EngineError::Internal(format!("listener join: {e}")))?;
    let _ = std::fs::remove_file(&opts.pid_file);
    let _ = std::fs::remove_file(&opts.readiness_file);
    listener_result
}

async fn wait_for_socket(p: &std::path::Path, total: Duration) -> crate::Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < total {
        if p.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(EngineError::Internal(format!(
        "socket {p:?} did not appear within {total:?}"
    )))
}
```

Add `pub mod daemon;` to `crates/ohara-engine/src/lib.rs`.

- [ ] **Step 3: Slim the CLI command.** Replace the body of `crates/ohara-cli/src/commands/serve.rs` — `ServeArgs` keeps its clap surface and gains one flag; `run` maps to the engine runner:

```rust
    /// Drop the lazily-loaded reranker session after this many seconds
    /// without a rerank. 0 disables idle unload.
    #[arg(long, default_value_t = 600)]
    pub reranker_idle_secs: u64,
```

```rust
pub async fn run(args: ServeArgs) -> Result<()> {
    ohara_engine::daemon::run_daemon(ohara_engine::daemon::DaemonOptions {
        socket: args.socket,
        pid_file: args.pid_file,
        readiness_file: args.readiness_file,
        idle_timeout_secs: args.idle_timeout,
        registry_path: args.registry_path,
        reranker_idle_secs: args.reranker_idle_secs,
    })
    .await
    .map_err(|e| anyhow::anyhow!("daemon: {e}"))
}
```

Delete the now-duplicated embedder construction, watchdogs, and `wait_for_socket` from `serve.rs`.

- [ ] **Step 4: Run**

Run: `cargo test -p ohara-engine && cargo test -p ohara-cli && cargo build -p ohara-cli`
Expected: green; `ohara serve --help` still shows the original flags plus `--reranker-idle-secs`.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/ohara-engine/src/daemon.rs crates/ohara-engine/src/lib.rs crates/ohara-cli/src/commands/serve.rs
git commit -m "refactor(engine): shared daemon runner with reranker idle-unload watchdog"
```

---

### Task 6: Remove the vestigial `busy` flag

Nothing sets `busy: true` in production; `pick_compatible` skipping busy daemons is a latent duplicate-spawn source once MCP traffic funnels through the daemon.

**Files:**
- Modify: `crates/ohara-engine/src/registry.rs` (drop field; drop skip; fix tests)
- Modify: `crates/ohara-engine/src/client/discover.rs` (record literal)
- Modify: `crates/ohara-cli/src/commands/daemon.rs` (List output)

- [ ] **Step 1: Remove `pub busy: bool` from `DaemonRecord`**, remove `&& !d.busy` from `pick_compatible`, delete the `pick_compatible_skips_busy_daemon` test, and remove `busy: false/true` from every record literal in `registry.rs` tests, `discover.rs` (`find_or_spawn_daemon`'s `DaemonRecord { ... }`), and update `pick_compatible_none_when_only_wrong_version_or_busy` to only cover the wrong-version case (rename to `pick_compatible_none_when_only_wrong_version`). In `crates/ohara-cli/src/commands/daemon.rs`, drop the `BUSY` column from the `List` header and row `println!`.

Old registry files containing `"busy": false` still deserialize (serde ignores unknown fields), and the registry path is per-version anyway.

- [ ] **Step 2: Run, then commit**

Run: `cargo test -p ohara-engine && cargo build --workspace`
Expected: green.

```bash
cargo fmt --all
git add crates/ohara-engine/src/registry.rs crates/ohara-engine/src/client/discover.rs crates/ohara-cli/src/commands/daemon.rs
git commit -m "refactor(engine): drop vestigial DaemonRecord.busy flag"
```

---

### Task 7: Atomic find-or-spawn under one registry lock

Today `find_or_spawn_daemon` does pick → spawn → register as three separate lock acquisitions; two concurrent cold starts can both spawn. Make the whole sequence run under one exclusive file lock.

**Files:**
- Modify: `crates/ohara-engine/src/registry.rs` (`locked_update`, `prune_dead`)
- Modify: `crates/ohara-engine/src/client/discover.rs` (use it)

- [ ] **Step 1: Write the failing test** (in `registry.rs` tests):

```rust
    #[test]
    fn locked_update_serialises_concurrent_pick_or_spawn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let spawns = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let spawns = spawns.clone();
            handles.push(std::thread::spawn(move || {
                let reg = Registry::open(&path).unwrap();
                reg.locked_update(|daemons| {
                    if let Some(existing) =
                        daemons.iter().find(|d| d.ohara_version == "0.9.0")
                    {
                        return existing.pid;
                    }
                    // Simulate a slow spawn while the lock is held.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    spawns.fetch_add(1, Ordering::SeqCst);
                    let rec = DaemonRecord {
                        pid: std::process::id(), // alive, survives prune
                        socket_path: PathBuf::from("/tmp/x.sock"),
                        ohara_version: "0.9.0".into(),
                        ohara_git_sha: None,
                        started_at_unix: 1,
                        last_health_unix: now_unix(),
                    };
                    daemons.push(rec.clone());
                    rec.pid
                })
                .unwrap()
            }));
        }
        let pids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "exactly one spawn");
        assert_eq!(pids[0], pids[1], "both callers see the same daemon");
    }
```

(`now_unix` is already a private fn in `registry.rs`; if it lives in `discover.rs`, add a sibling in `registry.rs`.)

Run: `cargo test -p ohara-engine locked_update` → COMPILE ERROR. Commit:

```bash
git add crates/ohara-engine/src/registry.rs
git commit -m "test(engine): registry locked_update serialises concurrent spawn (red)"
```

- [ ] **Step 2: Implement.** In `registry.rs`, extract the prune predicate from `list_alive` and add `locked_update`:

```rust
    /// Run `f` under the registry's exclusive file lock, after pruning
    /// dead/stale records. `f` may mutate the daemon list (e.g. push a
    /// freshly spawned record); mutations persist before the lock drops.
    ///
    /// The closure may block (the find-or-spawn path starts a daemon and
    /// waits for readiness, bounded at 10s); contention is rare — only
    /// concurrent cold starts collide here, and the loser then finds the
    /// winner's record instead of spawning a duplicate.
    pub fn locked_update<T>(&self, f: impl FnOnce(&mut Vec<DaemonRecord>) -> T) -> Result<T> {
        let mut out: Option<T> = None;
        self.mutate(|rf| {
            prune_dead(rf);
            out = Some(f(&mut rf.daemons));
            Ok(())
        })?;
        Ok(out.expect("invariant: mutate runs the closure exactly once"))
    }
```

```rust
fn prune_dead(rf: &mut RegistryFile) {
    let now = now_unix();
    rf.daemons
        .retain(|d| pid_alive(d.pid) && now.saturating_sub(d.last_health_unix) <= 5 * 60);
}
```

Rewrite `list_alive` to use the same primitive:

```rust
    pub fn list_alive(&self) -> Result<Vec<DaemonRecord>> {
        self.locked_update(|daemons| daemons.clone())
    }
```

- [ ] **Step 3: Rework `find_or_spawn_daemon`** in `discover.rs` — replace the pick/spawn/register sequence (keep the `no_daemon` and CI gates unchanged):

```rust
    let reg = Registry::open(registry_path)
        .map_err(|e| EngineError::Internal(format!("registry open: {e}")))?;

    enum Pick {
        Existing(DaemonRecord),
        Spawned(DaemonRecord),
        SpawnFailed(String),
    }

    let pick = reg
        .locked_update(|daemons| {
            if let Some(existing) = daemons.iter().find(|d| d.ohara_version == ohara_version) {
                return Pick::Existing(existing.clone());
            }
            match spawn_daemon(ohara_binary, &runtime_dir(), ohara_version, registry_path) {
                Ok(sd) => {
                    let rec = DaemonRecord {
                        pid: sd.pid,
                        socket_path: sd.socket_path,
                        ohara_version: ohara_version.into(),
                        ohara_git_sha: Some(ohara_git_sha.into()),
                        started_at_unix: now_unix(),
                        last_health_unix: now_unix(),
                    };
                    daemons.push(rec.clone());
                    Pick::Spawned(rec)
                }
                Err(e) => Pick::SpawnFailed(e.to_string()),
            }
        })
        .map_err(|e| EngineError::Internal(format!("registry locked_update: {e}")))?;

    match pick {
        Pick::Existing(d) => Ok(Some(DaemonHandle {
            socket_path: d.socket_path,
            pid: d.pid,
            spawned: false,
        })),
        Pick::Spawned(d) => Ok(Some(DaemonHandle {
            socket_path: d.socket_path,
            pid: d.pid,
            spawned: true,
        })),
        Pick::SpawnFailed(e) => Err(EngineError::Internal(format!("spawn daemon: {e}"))),
    }
```

The old orphan-kill-on-register-failure block is deleted — registration can no longer fail separately from the spawn. `pick_compatible` loses its last production caller: delete it and its remaining tests (its semantics live on inside the `locked_update` closure).

- [ ] **Step 4: Run, then commit**

Run: `cargo test -p ohara-engine`
Expected: green, including the new concurrency test.

```bash
cargo fmt --all
git add crates/ohara-engine/src/registry.rs crates/ohara-engine/src/client/discover.rs
git commit -m "fix(engine): atomic find-or-spawn — single registry lock kills duplicate daemon spawns"
```

---

### Task 8: Version sweep at daemon startup

After an upgrade, old-version daemons sit invisible in their per-version registries until idle timeout. Sweep them deterministically when a new daemon boots.

**Files:**
- Modify: `crates/ohara-engine/src/daemon.rs` (sweep fn + startup call)

- [ ] **Step 1: Write the failing test** (in `daemon.rs` tests):

```rust
    #[tokio::test]
    async fn sweep_removes_stale_version_dirs_with_dead_daemons() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path(); // plays the role of `<cache>/ohara/daemon`
        let stale = root.join("0.0.1");
        let current = root.join(env!("CARGO_PKG_VERSION"));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        // Dead-pid record in the stale-version registry.
        let reg = crate::registry::Registry::open(stale.join("registry.json")).unwrap();
        reg.register(crate::registry::DaemonRecord {
            pid: u32::MAX - 1, // not a live pid
            socket_path: stale.join("dead.sock"),
            ohara_version: "0.0.1".into(),
            ohara_git_sha: None,
            started_at_unix: 1,
            last_health_unix: 1,
        })
        .unwrap();

        sweep_stale_versions(root, env!("CARGO_PKG_VERSION")).await;

        assert!(!stale.exists(), "stale version dir must be removed");
        assert!(current.exists(), "current version dir must be untouched");
    }
```

Run: `cargo test -p ohara-engine sweep_removes` → COMPILE ERROR. Commit:

```bash
git add crates/ohara-engine/src/daemon.rs
git commit -m "test(engine): startup sweep removes stale-version daemon dirs (red)"
```

- [ ] **Step 2: Implement** in `daemon.rs`:

```rust
/// Best-effort cleanup of daemons left behind by other ohara versions.
///
/// `daemon_root` is the parent of the per-version registry dirs
/// (`<cache>/ohara/daemon`). For every sibling version dir: shut down
/// its live daemons over their sockets, then remove the dir once empty.
/// Failures are logged and skipped — the old daemons' own idle timeout
/// remains the backstop.
pub(crate) async fn sweep_stale_versions(daemon_root: &std::path::Path, current_version: &str) {
    let entries = match std::fs::read_dir(daemon_root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == current_version {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        let reg = match crate::registry::Registry::open(entry.path().join("registry.json")) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(dir = ?entry.path(), error = %e, "sweep: registry open failed");
                continue;
            }
        };
        let alive = match reg.list_alive() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let mut all_stopped = true;
        for d in alive {
            let req = crate::ipc::Request {
                id: 1,
                repo_path: None,
                method: crate::ipc::RequestMethod::Shutdown,
            };
            match crate::client::Client::connect(&d.socket_path).call(req).await {
                Ok(_) => {
                    let _ = reg.unregister(d.pid);
                    tracing::info!(pid = d.pid, "sweep: shut down stale-version daemon");
                }
                Err(e) => {
                    all_stopped = false;
                    tracing::warn!(pid = d.pid, error = %e, "sweep: shutdown failed");
                }
            }
        }
        if all_stopped {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
```

Wire it into `run_daemon_with_engine`, right after the readiness file is written (the registry path encodes `<root>/<version>/registry.json`):

```rust
    if let Some(reg_path) = &opts.registry_path {
        if let Some(daemon_root) = reg_path.parent().and_then(|p| p.parent()) {
            let root = daemon_root.to_path_buf();
            tokio::spawn(async move {
                sweep_stale_versions(&root, env!("CARGO_PKG_VERSION")).await;
            });
        }
    }
```

- [ ] **Step 3: Run, then commit**

Run: `cargo test -p ohara-engine`

```bash
cargo fmt --all
git add crates/ohara-engine/src/daemon.rs
git commit -m "feat(engine): sweep stale-version daemons at startup"
```

---

### Task 9: `ohara-mcp serve` mode

Plugin installs ship only `ohara-mcp`; the thin client (Task 10) spawns `current_exe() serve …`, so the MCP binary must host the daemon too.

**Files:**
- Modify: `crates/ohara-mcp/Cargo.toml` (`clap.workspace = true`)
- Modify: `crates/ohara-mcp/src/main.rs`

- [ ] **Step 1: Write the failing test** (in `main.rs`, bottom):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn serve_cli_parses_the_flag_shape_spawn_daemon_uses() {
        // Mirrors ohara-engine client/spawn.rs: socket, pid-file,
        // readiness-file, registry-path; idle flags default.
        let cli = ServeCli::try_parse_from([
            "ohara-mcp",
            "--socket", "/tmp/o.sock",
            "--pid-file", "/tmp/o.pid",
            "--readiness-file", "/tmp/o.ready",
            "--registry-path", "/tmp/registry.json",
        ])
        .unwrap();
        assert_eq!(cli.idle_timeout, 1800);
        assert_eq!(cli.reranker_idle_secs, 600);
        assert!(cli.registry_path.is_some());
    }
}
```

Run: `cargo test -p ohara-mcp serve_cli` → COMPILE ERROR. Commit:

```bash
git add crates/ohara-mcp/src/main.rs crates/ohara-mcp/Cargo.toml
git commit -m "test(mcp): serve-mode flag surface matches spawn_daemon (red)"
```

- [ ] **Step 2: Implement.** Add `clap.workspace = true` to `[dependencies]` in `crates/ohara-mcp/Cargo.toml`. Rework `main.rs`:

```rust
use anyhow::Result;
use clap::Parser;
use ohara_mcp::server;
use std::path::PathBuf;

/// Daemon-mode flags. Must accept exactly what
/// `ohara_engine::client::spawn::spawn_daemon` passes, since the thin
/// MCP client spawns `current_exe() serve …`.
#[derive(Parser, Debug)]
#[command(name = "ohara-mcp serve")]
struct ServeCli {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    pid_file: PathBuf,
    #[arg(long)]
    readiness_file: PathBuf,
    /// Exit after this many seconds with no requests. 0 disables.
    #[arg(long, default_value_t = 1800)]
    idle_timeout: u64,
    #[arg(long)]
    registry_path: Option<PathBuf>,
    /// Drop the reranker session after this many idle seconds. 0 disables.
    #[arg(long, default_value_t = 600)]
    reranker_idle_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug")),
        )
        .with_writer(std::io::stderr)
        .init();

    if std::env::args().nth(1).as_deref() == Some("serve") {
        return run_serve().await;
    }

    let workdir = std::env::current_dir()?;
    let server = server::OharaServer::open(workdir).await?;
    server.serve_stdio().await
}

async fn run_serve() -> Result<()> {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| "ohara-mcp".into());
    // Drop the literal "serve" so clap sees `argv0 --flags…`.
    let rest = std::env::args_os().skip(2);
    let cli = ServeCli::parse_from(std::iter::once(argv0).chain(rest));
    ohara_engine::daemon::run_daemon(ohara_engine::daemon::DaemonOptions {
        socket: cli.socket,
        pid_file: cli.pid_file,
        readiness_file: cli.readiness_file,
        idle_timeout_secs: cli.idle_timeout,
        registry_path: cli.registry_path,
        reranker_idle_secs: cli.reranker_idle_secs,
    })
    .await
    .map_err(|e| anyhow::anyhow!("daemon: {e}"))
}
```

(`unwrap_or_else` on env-filter and argv0 are existing/new non-test "infallible default" spots; the env-filter one is pre-existing code. If clippy's `unwrap_used` lint complains in this binary crate, keep the existing crate-level configuration — binaries already pass clippy with this pattern in `main.rs`.)

- [ ] **Step 3: Run, then commit**

Run: `cargo test -p ohara-mcp && cargo build -p ohara-mcp`

```bash
cargo fmt --all
git add crates/ohara-mcp/src/main.rs crates/ohara-mcp/Cargo.toml
git commit -m "feat(mcp): serve mode — ohara-mcp can host the shared daemon"
```

---

### Task 10: `ohara-mcp` thin client (daemon-first, lazy in-process fallback)

The big one. `OharaServer` stops loading models at boot; tools route to the shared daemon and fall back to a lazily-built in-process engine.

**Files:**
- Modify: `crates/ohara-mcp/src/server.rs`
- Modify: `crates/ohara-mcp/src/tools/find_pattern.rs`
- Modify: `crates/ohara-mcp/src/main.rs` (open() becomes sync)
- Modify: `crates/ohara-mcp/tests/envelope_parity.rs` (constructor)
- Create: `crates/ohara-mcp/tests/daemon_routing.rs`
- Modify: `crates/ohara-mcp/Cargo.toml` (dev-deps if missing: `tempfile.workspace`, `git2.workspace`, `tokio-util.workspace`)

- [ ] **Step 1: Rework `OharaServer`** (`server.rs`):

```rust
use anyhow::{Context, Result};
use ohara_engine::client::{find_or_spawn_daemon, registry_path, try_daemon_call, DaemonHandle};
use ohara_engine::ipc::{ErrorCode, Request, RequestMethod, Response};
use ohara_engine::RetrievalEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

pub struct OharaServer {
    pub repo_path: PathBuf,
    /// Lazily-built in-process engine; only constructed when the daemon
    /// path is unavailable AND a tool call actually needs the engine.
    fallback: OnceCell<Arc<RetrievalEngine>>,
    use_daemon: bool,
}

impl OharaServer {
    /// Plan-29: no model loading here — boot is path canonicalisation.
    /// `OHARA_NO_DAEMON=1` pins the in-process fallback (CI, debugging).
    pub fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        Ok(Self {
            repo_path: canonical,
            fallback: OnceCell::new(),
            use_daemon: std::env::var_os("OHARA_NO_DAEMON").is_none(),
        })
    }

    /// Test seam: a server that always uses `engine` in-process and never
    /// contacts a daemon (envelope-parity tests).
    pub fn with_engine(repo_path: PathBuf, engine: Arc<RetrievalEngine>) -> Self {
        Self {
            repo_path,
            fallback: OnceCell::new_with(Some(engine)),
            use_daemon: false,
        }
    }

    /// The in-process fallback engine. Lazy embedder + lazy reranker, so
    /// even building it loads no model; `explain_change` stays model-free.
    pub async fn engine(&self) -> &Arc<RetrievalEngine> {
        self.fallback
            .get_or_init(|| async {
                let embedder: Arc<dyn ohara_core::EmbeddingProvider> =
                    Arc::new(ohara_embed::LazyFastEmbedProvider::new());
                let reranker: Arc<dyn ohara_core::embed::RerankProvider> =
                    Arc::new(ohara_embed::LazyFastEmbedReranker::new());
                Arc::new(RetrievalEngine::new(embedder, reranker))
            })
            .await
    }

    /// Route one request to the shared daemon. `None` means "use the
    /// in-process fallback": daemon disabled, unreachable, or it answered
    /// `NotImplemented`. `OHARA_DAEMON_SOCKET` skips discovery (tests).
    pub async fn daemon_call(&self, method: RequestMethod) -> Option<Response> {
        if !self.use_daemon {
            return None;
        }
        let req = Request {
            id: 1,
            repo_path: Some(self.repo_path.to_string_lossy().to_string()),
            method,
        };
        if let Some(socket) = std::env::var_os("OHARA_DAEMON_SOCKET") {
            let h = DaemonHandle {
                socket_path: PathBuf::from(socket),
                pid: 0,
                spawned: false,
            };
            return filter_not_implemented(try_daemon_call(move || Ok(Some(h)), req).await);
        }
        let registry = registry_path().ok()?;
        let current_exe = std::env::current_exe().ok()?;
        let resp = try_daemon_call(
            move || {
                find_or_spawn_daemon(
                    &current_exe,
                    env!("CARGO_PKG_VERSION"),
                    option_env!("OHARA_GIT_SHA").unwrap_or("unknown"),
                    &registry,
                    false,
                )
            },
            req,
        )
        .await;
        filter_not_implemented(resp)
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }
}

/// `NotImplemented` means "this daemon can't do that yet" — treat it as
/// daemon-unavailable so the caller falls back in-process.
fn filter_not_implemented(resp: Option<Response>) -> Option<Response> {
    match resp {
        Some(r) if matches!(&r.error, Some(e) if e.code == ErrorCode::NotImplemented) => None,
        other => other,
    }
}
```

Keep the existing `compose_hint` function unchanged. Update `crates/ohara-mcp/src/main.rs`: `let server = server::OharaServer::open(workdir)?;` (drop `.await`).

Add unit tests at the bottom of `server.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod thin_client_tests {
    use super::*;
    use ohara_engine::ipc::ErrorPayload;

    fn resp(error: Option<ErrorPayload>) -> Response {
        Response { id: 1, result: Some(serde_json::json!({})), error }
    }

    #[test]
    fn not_implemented_becomes_none() {
        let r = resp(Some(ErrorPayload {
            code: ErrorCode::NotImplemented,
            message: "x".into(),
        }));
        assert!(filter_not_implemented(Some(r)).is_none());
    }

    #[test]
    fn other_errors_pass_through() {
        let r = resp(Some(ErrorPayload {
            code: ErrorCode::NeedsRebuild,
            message: "x".into(),
        }));
        assert!(filter_not_implemented(Some(r)).is_some());
        assert!(filter_not_implemented(Some(resp(None))).is_some());
        assert!(filter_not_implemented(None).is_none());
    }
}
```

- [ ] **Step 2: Rework the `find_pattern` tool** (`tools/find_pattern.rs`). Delete the local compatibility pre-check block (the `open_repo`/`get_index_metadata`/`assess`/refuse section — the engine guard from Task 4 replaces it) and the now-unused imports (`CompatibilityStatus`, `current_runtime_metadata`). New handler body after `parse_since`:

```rust
        let q = ohara_core::query::PatternQuery {
            query: input.query.clone(),
            k: input.k.clamp(1, 20),
            language: input.language,
            since_unix,
            no_rerank: input.no_rerank,
        };

        // Daemon-first (plan-29): one shared engine across sessions.
        if let Some(resp) = self
            .server
            .daemon_call(RequestMethod::FindPattern(q.clone()))
            .await
        {
            return find_pattern_body(decode_find_pattern(resp)?, &input.query);
        }

        // In-process fallback (daemon disabled or unreachable).
        let engine = self.server.engine().await;
        let result = engine
            .find_pattern(&self.server.repo_path, q)
            .await
            .map_err(map_engine_error)?;
        find_pattern_body(result, &input.query)
    }
```

With these module-level helpers (and unit tests pinning the refusal wording):

```rust
use ohara_engine::ipc::{ErrorCode, RequestMethod, Response};
use ohara_engine::FindPatternResult;

fn rebuild_refusal(detail: &str) -> rmcp::Error {
    rmcp::Error::invalid_params(
        format!(
            "find_pattern refuses to run: {detail}. \
             Run `ohara index --rebuild` in this repo first."
        ),
        None,
    )
}

fn map_engine_error(e: ohara_engine::EngineError) -> rmcp::Error {
    match e {
        ohara_engine::EngineError::NeedsRebuild { reason } => {
            rebuild_refusal(&format!("index needs rebuild ({reason})"))
        }
        other => rmcp::Error::internal_error(other.to_string(), None),
    }
}

/// Daemon response → typed result, mapping NeedsRebuild to the same
/// refusal envelope the in-process path produces.
fn decode_find_pattern(resp: Response) -> Result<FindPatternResult, rmcp::Error> {
    if let Some(err) = resp.error {
        if err.code == ErrorCode::NeedsRebuild {
            return Err(rebuild_refusal(&err.message));
        }
        return Err(rmcp::Error::internal_error(err.message, None));
    }
    let value = resp
        .result
        .ok_or_else(|| rmcp::Error::internal_error("daemon response missing result".to_string(), None))?;
    serde_json::from_value(value)
        .map_err(|e| rmcp::Error::internal_error(format!("decode FindPatternResult: {e}"), None))
}

/// Shared envelope builder — both paths produce the existing wire shape.
fn find_pattern_body(
    result: FindPatternResult,
    query_text: &str,
) -> Result<CallToolResult, rmcp::Error> {
    let parsed = parse_query(query_text);
    let profile = RetrievalProfile::for_intent(parsed.intent);
    let meta = result.meta;
    let body = json!({
        "hits": result.hits,
        "_meta": {
            "index_status": meta.index_status,
            "hint": meta.hint,
            "compatibility": meta.compatibility,
            "query_profile": {
                "name": profile.name,
                "explanation": profile.explanation,
            },
        }
    });
    Ok(CallToolResult::success(vec![Content::text(
        body.to_string(),
    )]))
}
```

Unit tests (same file, tests module):

```rust
    #[test]
    fn decode_find_pattern_maps_needs_rebuild_to_refusal() {
        use ohara_engine::ipc::{ErrorCode, ErrorPayload, Response};
        let resp = Response {
            id: 1,
            result: None,
            error: Some(ErrorPayload {
                code: ErrorCode::NeedsRebuild,
                message: "index needs rebuild: embedding_model mismatch".into(),
            }),
        };
        let err = decode_find_pattern(resp).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("needs rebuild"), "got: {msg}");
        assert!(msg.contains("ohara index --rebuild"), "got: {msg}");
    }
```

- [ ] **Step 3: Rework the `explain_change` handler** (same impl block). Keep the local `line_start`/`line_end` resolution. Replace everything from building `q` onward:

```rust
        let q = ohara_core::explain::ExplainQuery {
            file: input.file,
            line_start,
            line_end,
            k: input.k.clamp(1, 20),
            include_diff: input.include_diff,
            include_related: false,
        };

        // Daemon-first: blame result + index status both from the shared
        // engine. Any miss → full in-process fallback below.
        if let Some(resp) = self
            .server
            .daemon_call(RequestMethod::ExplainChange(q.clone()))
            .await
        {
            let explain: Option<ohara_engine::ExplainResult> = decode_ok(resp);
            let meta: Option<ohara_core::query::ResponseMeta> =
                match self.server.daemon_call(RequestMethod::IndexStatus).await {
                    Some(m) => decode_ok(m),
                    None => None,
                };
            if let (Some(explain), Some(meta)) = (explain, meta) {
                let body = json!({
                    "hits": explain.hits,
                    "_meta": {
                        "index_status": meta.index_status,
                        "hint": meta.hint,
                        "explain": explain.meta,
                    }
                });
                return Ok(CallToolResult::success(vec![Content::text(
                    body.to_string(),
                )]));
            }
        }

        // In-process fallback — existing logic, on the lazy engine.
        let engine = self.server.engine().await;
        let explain_result = engine
            .explain_change(&self.server.repo_path, q)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let handle = engine
            .open_repo(&self.server.repo_path)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        // … (keep the existing index-status/compatibility/hint block and
        // the existing body construction verbatim from the current file)
```

Plus the small decode helper next to the others:

```rust
/// Successful daemon response → typed value; anything else → None
/// (caller falls back in-process).
fn decode_ok<T: serde::de::DeserializeOwned>(resp: Response) -> Option<T> {
    if resp.error.is_some() {
        return None;
    }
    serde_json::from_value(resp.result?).ok()
}
```

- [ ] **Step 4: Update the envelope-parity tests.** In `crates/ohara-mcp/tests/envelope_parity.rs`, `make_server` currently ends by constructing `OharaServer { repo_path, engine }` as a struct literal (fields were pub). Replace that final expression with:

```rust
    OharaServer::with_engine(canonical, Arc::new(engine))
```

keeping its existing engine construction (stub providers + indexed fixture). Adjust the variable names to whatever the helper already uses — the only change is *how* the server is assembled; `with_engine` guarantees the in-process path, so the goldens exercise the same code as before.

- [ ] **Step 5: Run the red suite**

Run: `cargo test -p ohara-mcp`
Expected: compiles and existing envelope goldens PASS (fallback path is the old path). If a golden diverges, the body builder drifted — fix the builder, not the golden.

Commit:

```bash
cargo fmt --all
git add crates/ohara-mcp/src/server.rs crates/ohara-mcp/src/tools/find_pattern.rs crates/ohara-mcp/src/main.rs crates/ohara-mcp/tests/envelope_parity.rs
git commit -m "feat(mcp): thin daemon-first client; boot loads no models"
```

- [ ] **Step 6: Daemon-routing integration test.** Create `crates/ohara-mcp/tests/daemon_routing.rs` — a REAL daemon (serve_unix + stub-provider engine) on a temp socket, reached via `OHARA_DAEMON_SOCKET`:

```rust
//! Plan-29: the MCP tools route through a daemon when one is reachable.
//! Spins a real `serve_unix` listener with stub providers and points the
//! thin client at it via OHARA_DAEMON_SOCKET.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ohara_core::EmbeddingProvider;
use ohara_core::embed::RerankProvider;
use ohara_mcp::server::OharaServer;
use ohara_mcp::tools::find_pattern::{FindPatternInput, OharaService};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct DummyEmbedder;
#[async_trait::async_trait]
impl EmbeddingProvider for DummyEmbedder {
    fn dimension(&self) -> usize { 384 }
    fn model_id(&self) -> &str { "dummy" }
    async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
    }
}
struct DummyReranker;
#[async_trait::async_trait]
impl RerankProvider for DummyReranker {
    async fn rerank(&self, _q: &str, c: &[&str]) -> ohara_core::Result<Vec<f32>> {
        Ok(vec![0.0; c.len()])
    }
}

fn build_repo(dir: &Path) {
    use git2::{Repository, Signature};
    let repo = Repository::init(dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn one() {}\n").unwrap();
    let sig = Signature::now("a", "a@a").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("a.rs")).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).unwrap();
}

#[tokio::test]
async fn find_pattern_routes_through_daemon_socket() {
    let ohara_home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", ohara_home.path());
    build_repo(repo.path());
    let canonical = std::fs::canonicalize(repo.path()).unwrap();

    // Index so the daemon-side engine has storage to query.
    {
        let walker = ohara_git::GitWalker::open(&canonical).unwrap();
        let first = walker.first_commit_sha().unwrap();
        let repo_id =
            ohara_core::types::RepoId::from_parts(&first, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id).unwrap();
        let storage: Arc<dyn ohara_core::Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await.unwrap());
        let commit_src = Arc::new(ohara_git::GitCommitSource::open(&canonical).unwrap());
        let symbol_src = Arc::new(ohara_parse::GitSymbolSource::open(&canonical).unwrap());
        let indexer = ohara_core::Indexer::new(storage, Arc::new(DummyEmbedder));
        indexer.run(&repo_id, commit_src, symbol_src).await.unwrap();
    }

    // Real daemon on a temp socket.
    let engine = Arc::new(ohara_engine::RetrievalEngine::new(
        Arc::new(DummyEmbedder),
        Arc::new(DummyReranker),
    ));
    let sock = sock_dir.path().join("ohara.sock");
    let stop = CancellationToken::new();
    let daemon = {
        let (engine, sock, stop) = (engine, sock.clone(), stop.clone());
        tokio::spawn(async move { ohara_engine::server::serve_unix(engine, &sock, stop).await })
    };
    for _ in 0..100 {
        if sock.exists() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    std::env::set_var("OHARA_DAEMON_SOCKET", &sock);

    let server = OharaServer::open(&canonical).unwrap();
    let svc = OharaService::new(server);
    let out = svc
        .find_pattern(FindPatternInput {
            query: "one".into(),
            k: 5,
            language: None,
            since: None,
            no_rerank: true,
        })
        .await
        .expect("daemon-routed find_pattern");
    let text = match &out.content[0].raw {
        rmcp::model::RawContent::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v.get("hits").is_some(), "envelope must have hits: {v}");
    assert!(v["_meta"].get("index_status").is_some(), "envelope must have _meta.index_status: {v}");

    std::env::remove_var("OHARA_DAEMON_SOCKET");
    stop.cancel();
    let _ = daemon.await;
}
```

Notes for the executor: (a) `serve_unix` must be reachable — `lib.rs` already exports `pub use server::serve_unix`, and the test may use that re-export instead of `ohara_engine::server::serve_unix` if the module is private; (b) the `CallToolResult` content-extraction lines must match how `envelope_parity.rs::result_to_value` does it — copy that helper if the `raw` shape differs in rmcp 0.1.5; (c) env-var mutation: this is the only test in the file, so no env lock is needed, but if more tests are added later, mirror the `env_lock()` pattern; (d) add `git2.workspace = true`, `tempfile.workspace = true`, `tokio-util.workspace = true` to `[dev-dependencies]` in `crates/ohara-mcp/Cargo.toml` if not already present (check — envelope_parity likely pulls most of these already); (e) `OharaService::new(server)` and the tools must be importable from the lib target — `find_pattern` module is already `pub`.

- [ ] **Step 7: Run everything, then commit**

Run: `cargo test -p ohara-mcp`
Expected: envelope goldens + daemon_routing + unit tests all green.

```bash
cargo fmt --all
git add crates/ohara-mcp/tests/daemon_routing.rs crates/ohara-mcp/Cargo.toml
git commit -m "test(mcp): daemon-socket routing integration test"
```

---

### Task 11: B1 — plugin wrapper reads its version from plugin.json (+ cache sweep + CI drift check)

**Files:**
- Modify: `plugins/ohara/bin/ohara-mcp`
- Modify: `plugins/ohara/.claude-plugin/plugin.json` (`"version": "0.9.0"`)
- Modify: `plugins/ohara/package.json` (`"version": "0.9.0"`)
- Create: `plugins/ohara/test/wrapper.test.js`
- Modify: `.github/workflows/ci.yml` (drift check + wrapper tests)

- [ ] **Step 1: Write the failing tests** — `plugins/ohara/test/wrapper.test.js`:

```js
'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');

const { resolveVersion, sweepOldCaches } = require('../bin/ohara-mcp');

test('resolveVersion honours OHARA_PLUGIN_VERSION override', () => {
  process.env.OHARA_PLUGIN_VERSION = '1.2.3';
  try {
    assert.strictEqual(resolveVersion(), '1.2.3');
  } finally {
    delete process.env.OHARA_PLUGIN_VERSION;
  }
});

test('resolveVersion falls back to plugin.json', () => {
  delete process.env.OHARA_PLUGIN_VERSION;
  const manifest = JSON.parse(
    fs.readFileSync(path.join(__dirname, '..', '.claude-plugin', 'plugin.json'), 'utf8')
  );
  assert.strictEqual(resolveVersion(), manifest.version);
});

test('sweepOldCaches removes only other v* dirs', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ohara-sweep-'));
  fs.mkdirSync(path.join(root, 'v0.7.4'));
  fs.mkdirSync(path.join(root, 'v0.9.0'));
  fs.writeFileSync(path.join(root, 'not-a-version'), '');
  sweepOldCaches(root, 'v0.9.0');
  assert.ok(!fs.existsSync(path.join(root, 'v0.7.4')), 'old version dir removed');
  assert.ok(fs.existsSync(path.join(root, 'v0.9.0')), 'current kept');
  assert.ok(fs.existsSync(path.join(root, 'not-a-version')), 'non-version entries kept');
});
```

Run: `node --test plugins/ohara/test`
Expected: FAIL — requiring `../bin/ohara-mcp` runs `main()` (no exports, side effects). Commit:

```bash
git add plugins/ohara/test/wrapper.test.js
git commit -m "test(plugin): wrapper version resolution + cache sweep (red)"
```

- [ ] **Step 2: Rework the wrapper.** In `plugins/ohara/bin/ohara-mcp`:

Replace the hardcoded version constant:

```js
function resolveVersion() {
  if (process.env.OHARA_PLUGIN_VERSION) return process.env.OHARA_PLUGIN_VERSION;
  const manifest = path.join(__dirname, '..', '.claude-plugin', 'plugin.json');
  return JSON.parse(fs.readFileSync(manifest, 'utf8')).version;
}

const OHARA_VERSION = resolveVersion();
```

Add the sweep next to `ensureBinary` and call it after a successful fresh download (inside `ensureBinary`, right after `fs.writeFileSync(cachedSentinel, resolved);`):

```js
function sweepOldCaches(cacheRoot, keep) {
  let entries;
  try {
    entries = fs.readdirSync(cacheRoot, { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of entries) {
    if (!e.isDirectory()) continue;
    if (!e.name.startsWith('v')) continue;
    if (e.name === keep) continue;
    fs.rmSync(path.join(cacheRoot, e.name), { recursive: true, force: true });
  }
}
```

```js
  fs.writeFileSync(cachedSentinel, resolved);
  sweepOldCaches(CACHE_ROOT, `v${OHARA_VERSION}`);
  return resolved;
```

Guard execution and export for tests (replace the bare `main().catch(...)` at the bottom):

```js
if (require.main === module) {
  main().catch((err) => {
    process.stderr.write(`[ohara-plugin] ${err && err.message ? err.message : err}\n`);
    process.exit(1);
  });
}

module.exports = { resolveVersion, sweepOldCaches, targetTriple, archiveName };
```

Bump `"version"` to `"0.9.0"` in both `plugins/ohara/.claude-plugin/plugin.json` and `plugins/ohara/package.json`.

- [ ] **Step 3: Run**

Run: `node --test plugins/ohara/test`
Expected: 3 pass.

- [ ] **Step 4: CI drift check.** In `.github/workflows/ci.yml`, append a step to the `build-test` job (after the nextest step; ubuntu-24.04 ships node ≥18):

```yaml
      - name: Plugin wrapper tests + version drift check
        run: |
          node --test plugins/ohara/test
          CARGO_V=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
          PLUGIN_V=$(node -p "require('./plugins/ohara/.claude-plugin/plugin.json').version")
          if [ "$CARGO_V" != "$PLUGIN_V" ]; then
            echo "::error::plugin.json version ($PLUGIN_V) != workspace version ($CARGO_V) — bump plugins/ohara/{.claude-plugin/plugin.json,package.json} in the release commit"
            exit 1
          fi
```

Verify locally that `grep -m1 '^version' Cargo.toml | cut -d'"' -f2` prints `0.9.0` (the workspace version lives in root `Cargo.toml` under `[workspace.package]`); if the first `version` line in the root manifest is something else, anchor the grep accordingly before committing.

- [ ] **Step 5: Commit**

```bash
git add plugins/ohara/bin/ohara-mcp plugins/ohara/.claude-plugin/plugin.json plugins/ohara/package.json .github/workflows/ci.yml
git commit -m "fix(plugin): resolve binary version from plugin.json; sweep stale caches; CI drift gate"
```

---

### Task 12: Docs, full verification, manual e2e

**Files:**
- Modify: `CLAUDE.md` (two lines)
- No other code.

- [ ] **Step 1: Update CLAUDE.md.** In the Architecture cheatsheet's MCP bullet, replace:

> The server treats the spawning client's CWD as the repo to query.

with:

> The server treats the spawning client's CWD as the repo to query. Since plan-29 it is a thin client: tool calls route to the shared `ohara serve` daemon (spawning `ohara-mcp serve` itself if none runs); `OHARA_NO_DAEMON=1` pins the lazy in-process fallback.

- [ ] **Step 2: Full pre-completion checklist (CONTRIBUTING §13)**

```bash
cargo fmt --all                 # must be a no-op
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
node --test plugins/ohara/test
awk 'END { if (NR >= 500) exit 1 }' crates/ohara-engine/src/daemon.rs   # <500 lines
```

All green. Then:

```bash
git add CLAUDE.md
git commit -m "docs: MCP server is a thin daemon client (plan-29)"
```

- [ ] **Step 3: Manual e2e (operator)**

```bash
cargo build --release
# Terminal A and B, in two different indexed repos:
target/release/ohara-mcp   # (driven by an MCP client, or use `ohara query` twice from two shells)
# Then verify exactly one daemon:
target/release/ohara daemon status        # one row
ps aux | grep -i 'ohara' | grep -v grep   # MCP processes small; one fat daemon
# Wait >10 min idle, re-check daemon RSS dropped (reranker unloaded).
```

Record the before/after RSS numbers in the PR description.

---

## Self-review notes (already applied)

- **Spec coverage:** A1 → Tasks 3, 5, 9, 10; A2 → Tasks 6, 7, 8; A3 → Tasks 1, 2, 4, 5; B1 → Task 11. A4 (`Embed` IPC) is wave 2 — intentionally not in this plan.
- **Type consistency:** `DaemonOptions` field names match between Tasks 5/9; `unload_if_idle(Duration) -> bool` consistent across trait (Task 2), engine (Task 4), watchdog (Task 5); `DaemonRecord` literals after Task 6 have no `busy` field — Tasks 7/8 record literals were written accordingly.
- **Known soft spots for the executor:** rmcp 0.1.5 content extraction in `daemon_routing.rs` (mirror `envelope_parity.rs`); the exact final expression of `make_server`; clippy `unwrap_used` posture in binary `main.rs` files. Each is flagged inline where it occurs.
