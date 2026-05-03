# ohara plan-14 — perf harness and tracing (Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** land the measurement substrate for the v0.7+ CLI/MCP
performance program — phase-level tracing, per-method storage
metrics, opt-in SQL trace, a ripgrep medium fixture, and three
operator-run harness binaries that emit JSON for PR descriptions.

**Architecture:** add a `phase` tracing target in `ohara-core` with a
single `timed_phase` helper that wraps async sections and emits one
event per phase; add `AtomicU64` counters into `SqliteStorage` and
expose them via a new `Storage::metrics_snapshot` trait method;
install a `Connection::trace` callback gated by
`RUST_LOG=ohara_storage::sql=trace`; add a `--trace-perf` CLI flag
that installs a `tracing-subscriber` layer captures phase events and
dumps them to stderr at process exit; add a `fixtures/build_medium.sh`
script that shallow-clones ripgrep at tag `14.1.1`; add three new
test-binaries under `tests/perf/` that exercise CLI + in-process MCP
paths and emit JSON to `target/perf/runs/`.

**Tech Stack:** Rust 2021, `tracing` + `tracing-subscriber`, `rusqlite`
trace API, `serde_json`, existing `tests/perf` workspace member,
`#[ignore]` test-binary pattern.

**Spec:** `docs/superpowers/specs/2026-05-03-ohara-cli-mcp-perf-design.md`
§Phase 1.

**Scope check:** This plan implements Phase 1 only. Phases 2–4 of the
spec ship as separate plans (`plan-15`, `plan-16`, `plan-17`) ordered
by what Phase 1's harness reveals.

---

## Phase A — Tracing layer

### Task A.1 — `timed_phase` helper in `ohara-core`

**Files:**
- Create: `crates/ohara-core/src/perf_trace.rs`
- Modify: `crates/ohara-core/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

Append to `crates/ohara-core/src/perf_trace.rs` (created in Step 2):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::TestWriter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    /// Capture every event emitted on `target = "ohara::phase"` into a
    /// `Vec<(phase_name, elapsed_ms_present)>` so the test can assert
    /// the helper actually fires the right shape of event.
    #[derive(Default, Clone)]
    struct PhaseCaptor {
        events: std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PhaseCaptor {
        fn on_event(
            &self,
            ev: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if ev.metadata().target() != "ohara::phase" {
                return;
            }
            let mut phase = String::new();
            let mut has_elapsed = false;
            struct V<'a> {
                phase: &'a mut String,
                has_elapsed: &'a mut bool,
            }
            impl<'a> tracing::field::Visit for V<'a> {
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    if f.name() == "phase" {
                        *self.phase = v.to_string();
                    }
                }
                fn record_u64(&mut self, f: &tracing::field::Field, _: u64) {
                    if f.name() == "elapsed_ms" {
                        *self.has_elapsed = true;
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            ev.record(&mut V {
                phase: &mut phase,
                has_elapsed: &mut has_elapsed,
            });
            self.events.lock().unwrap().push((phase, has_elapsed));
        }
    }

    #[tokio::test]
    async fn timed_phase_emits_one_event_with_phase_and_elapsed_ms() {
        let cap = PhaseCaptor::default();
        let sub = Registry::default().with(cap.clone());
        let out = with_default(sub, || {
            futures::executor::block_on(async {
                timed_phase("lane_knn", async { 42_u32 }).await
            })
        });
        assert_eq!(out, 42);
        let events = cap.events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one phase event per call");
        assert_eq!(events[0].0, "lane_knn");
        assert!(events[0].1, "elapsed_ms must be recorded");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
cargo test -p ohara-core --lib perf_trace::tests::timed_phase_emits_one_event_with_phase_and_elapsed_ms
```

Expected: FAIL with "could not find `perf_trace` in the crate root" or
"`timed_phase` is not defined".

- [ ] **Step 3: Implement the helper.**

Replace the contents of `crates/ohara-core/src/perf_trace.rs` with:

```rust
//! Phase-level tracing helper. Every async section we want timed in
//! the perf harness is wrapped with [`timed_phase`]; the helper emits
//! exactly one `tracing::info!` event on target `ohara::phase` with
//! `phase = <name>` and `elapsed_ms = <u64>`.
//!
//! The harness installs a `tracing-subscriber` layer that filters on
//! `target == "ohara::phase"` and aggregates by phase name. End users
//! never see these events unless they pass `--trace-perf` or set
//! `RUST_LOG=ohara::phase=info`.

use std::future::Future;
use std::time::Instant;

/// Run `fut` and emit a phase event capturing its elapsed time.
///
/// `name` is a `'static` so the subscriber can use it as a stable
/// aggregation key; phase names are part of the perf harness contract
/// (see the spec's tracing schema) and adding a new one is a real
/// product change, not an ad-hoc literal.
pub async fn timed_phase<T, F: Future<Output = T>>(name: &'static str, fut: F) -> T {
    let start = Instant::now();
    let out = fut.await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::info!(target: "ohara::phase", phase = name, elapsed_ms);
    out
}

/// Like [`timed_phase`] but additionally records `hit_count` for lane
/// queries / rerank stages where row count is part of the harness
/// signal.
pub async fn timed_phase_with_count<T, F: Future<Output = (T, usize)>>(
    name: &'static str,
    fut: F,
) -> T {
    let start = Instant::now();
    let (out, count) = fut.await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "ohara::phase",
        phase = name,
        elapsed_ms,
        hit_count = count as u64,
    );
    out
}
```

- [ ] **Step 4: Re-export from `lib.rs`.**

Add to `crates/ohara-core/src/lib.rs` (after the existing `pub mod`
declarations):

```rust
pub mod perf_trace;
```

- [ ] **Step 5: Add `futures` to ohara-core dev-deps for the test.**

Edit `crates/ohara-core/Cargo.toml` `[dev-dependencies]` section.
Add (alphabetically, near `tokio`):

```toml
futures = "0.3"
tracing-subscriber.workspace = true
```

If `tracing-subscriber` is not already in workspace dev-deps, add it
under `[workspace.dependencies]` in the root `Cargo.toml` (it is; see
the existing workspace deps).

- [ ] **Step 6: Run the test and verify it passes.**

```bash
cargo test -p ohara-core --lib perf_trace::tests::timed_phase_emits_one_event_with_phase_and_elapsed_ms
```

Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add crates/ohara-core/src/perf_trace.rs \
        crates/ohara-core/src/lib.rs \
        crates/ohara-core/Cargo.toml
git commit -m "feat(core): add timed_phase helper for perf tracing"
```

---

### Task A.2 — Wrap retriever phases with `timed_phase`

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs:106-310`

- [ ] **Step 1: Add a failing test asserting all expected phase names fire.**

Append to the existing `mod tests` block in
`crates/ohara-core/src/retriever.rs` (before the closing `}`):

```rust
    #[tokio::test]
    async fn find_pattern_emits_expected_phase_events() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;

        #[derive(Default, Clone)]
        struct PhaseSet {
            seen: Arc<Mutex<std::collections::BTreeSet<String>>>,
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PhaseSet {
            fn on_event(
                &self,
                ev: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if ev.metadata().target() != "ohara::phase" {
                    return;
                }
                struct V<'a>(&'a mut Option<String>);
                impl<'a> tracing::field::Visit for V<'a> {
                    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                        if f.name() == "phase" {
                            *self.0 = Some(v.to_string());
                        }
                    }
                    fn record_u64(&mut self, _: &tracing::field::Field, _: u64) {}
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                }
                let mut name: Option<String> = None;
                ev.record(&mut V(&mut name));
                if let Some(n) = name {
                    self.seen.lock().unwrap().insert(n);
                }
            }
        }

        let now = 1_700_000_000;
        let knn = vec![fake_hit(1, "a", now, 0.9, "diff-a")];
        let fts = vec![fake_hit(1, "a", now, 0.7, "diff-a")];
        let storage = Arc::new(FakeStorage::new(knn, fts, vec![]));
        let embedder = Arc::new(FakeEmbedder);
        let r = Retriever::new(storage, embedder);
        let q = PatternQuery {
            query: "anything".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: true,
        };
        let id = RepoId::from_parts("x", "/y");

        let cap = PhaseSet::default();
        let sub = Registry::default().with(cap.clone());
        with_default(sub, || {
            futures::executor::block_on(async {
                r.find_pattern(&id, &q, now).await.unwrap();
            })
        });
        let seen = cap.seen.lock().unwrap();
        for required in [
            "embed_query",
            "lane_knn",
            "lane_fts_text",
            "lane_fts_sym_hist",
            "lane_fts_sym_head",
            "rrf",
            "hydrate_symbols",
        ] {
            assert!(
                seen.contains(required),
                "missing phase event {required}; seen = {:?}",
                *seen
            );
        }
    }
```

- [ ] **Step 2: Run the test to confirm it fails.**

```bash
cargo test -p ohara-core --lib retriever::tests::find_pattern_emits_expected_phase_events
```

Expected: FAIL — phase events do not yet fire.

- [ ] **Step 3: Wrap each phase in `find_pattern_with_profile`.**

Edit `crates/ohara-core/src/retriever.rs`. Add at the top of the file
(near the existing `use` block):

```rust
use crate::perf_trace::{timed_phase, timed_phase_with_count};
```

Inside `find_pattern_with_profile`, replace the relevant blocks:

Section 1 (replace the embed call around line 125):
```rust
        let q_text = vec![query.query.clone()];
        let mut q_embs = timed_phase("embed_query", self.embedder.embed_batch(&q_text)).await?;
```

Section 2 (replace the `tokio::join!` lane gather around line 141 with
per-lane wrapping; keep the four lanes parallel):
```rust
        let (vec_res, fts_res, hist_sym_res, head_sym_res) = tokio::join!(
            timed_phase("lane_knn", self.storage.knn_hunks(
                repo_id,
                &q_emb,
                self.weights.lane_top_k,
                language_filter,
                query.since_unix.or(parsed.since_unix),
            )),
            timed_phase("lane_fts_text", self.storage.bm25_hunks_by_text(
                repo_id,
                &query.query,
                self.weights.lane_top_k,
                language_filter,
                query.since_unix.or(parsed.since_unix),
            )),
            timed_phase("lane_fts_sym_hist", self.storage.bm25_hunks_by_historical_symbol(
                repo_id,
                &query.query,
                self.weights.lane_top_k,
                language_filter,
                query.since_unix.or(parsed.since_unix),
            )),
            timed_phase("lane_fts_sym_head", self.storage.bm25_hunks_by_symbol_name(
                repo_id,
                &query.query,
                self.weights.lane_top_k,
                language_filter,
                query.since_unix.or(parsed.since_unix),
            )),
        );
```

Section 4 (RRF — wrap the `reciprocal_rank_fusion` call around line 224):
```rust
        let fused: Vec<HunkId> = timed_phase("rrf", async {
            reciprocal_rank_fusion(&[ranking_vec, ranking_fts, ranking_sym], 60)
        })
        .await;
```

Section 5 (rerank — wrap the rerank call around line 240):
```rust
        let rerank_scores: Vec<f32> = match (&self.reranker, query.no_rerank) {
            (Some(r), false) => {
                timed_phase("rerank", r.rerank(&query.query, &candidates)).await?
            }
            _ => vec![1.0_f32; candidates.len()],
        };
```

Section 6 (hydrate symbols — wrap the per-hit loop around line 256):
```rust
        let symbols_by_hunk = timed_phase("hydrate_symbols", async {
            let mut acc: std::collections::HashMap<HunkId, Vec<String>> =
                std::collections::HashMap::new();
            for h in &hits {
                let attrs = self.storage.get_hunk_symbols(repo_id, h.hunk_id).await?;
                if !attrs.is_empty() {
                    acc.insert(h.hunk_id, attrs.into_iter().map(|a| a.name).collect());
                }
            }
            Ok::<_, crate::OhraError>(acc)
        })
        .await?;
```

Note: `timed_phase_with_count` is left unused in this task because
the lane-result row-count surfaces naturally on the response side; we
use the simpler `timed_phase` consistently here. The
`timed_phase_with_count` helper is retained for explicit hit-count
phases added later (e.g., the harness binaries' own outer span).

- [ ] **Step 4: Run the test and verify it passes.**

```bash
cargo test -p ohara-core --lib retriever::tests::find_pattern_emits_expected_phase_events
```

Expected: PASS.

- [ ] **Step 5: Run the full retriever test module to verify no regressions.**

```bash
cargo test -p ohara-core --lib retriever::
```

Expected: all PASS, including the four pre-existing tests
(`truncate_*`, `find_pattern_invokes_three_lanes_and_rrf`,
`find_pattern_no_rerank_returns_post_rrf_top_k`,
`find_pattern_query_no_rerank_flag_skips_attached_reranker`,
`find_pattern_recency_multiplier_breaks_ties_when_no_rerank`).

- [ ] **Step 6: Commit.**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "feat(core): wrap retrieval phases with timed_phase"
```

---

### Task A.3 — Wrap explain phases with `timed_phase`

**Files:**
- Modify: `crates/ohara-core/src/explain.rs`

- [ ] **Step 1: Locate the current `explain_change` orchestration.**

Open `crates/ohara-core/src/explain.rs`. The function
`pub async fn explain_change(...)` is the entry point used by both
the CLI (`ohara explain`) and the MCP `explain_change` tool.

- [ ] **Step 2: Add a failing test that captures phase events.**

Append to the existing `mod tests` block:

```rust
    #[tokio::test]
    async fn explain_change_emits_blame_and_hydrate_phases() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;

        #[derive(Default, Clone)]
        struct PhaseSet(Arc<Mutex<std::collections::BTreeSet<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PhaseSet {
            fn on_event(
                &self,
                ev: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if ev.metadata().target() != "ohara::phase" {
                    return;
                }
                struct V<'a>(&'a mut Option<String>);
                impl<'a> tracing::field::Visit for V<'a> {
                    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                        if f.name() == "phase" {
                            *self.0 = Some(v.to_string());
                        }
                    }
                    fn record_u64(&mut self, _: &tracing::field::Field, _: u64) {}
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                }
                let mut name = None;
                ev.record(&mut V(&mut name));
                if let Some(n) = name {
                    self.0.lock().unwrap().insert(n);
                }
            }
        }

        // The test reuses whatever fake/blamer fixture the existing
        // explain tests use; if the fixture builder isn't yet exposed
        // outside the tests module, lift it. Concretely, look for a
        // helper named `tiny_explain_fixture` (or similar) elsewhere
        // in this file's `mod tests`. If absent, follow the pattern
        // in retriever::tests::FakeStorage and write a minimal
        // `FakeBlamer` returning a single blame line.
        let (storage, blamer, repo_id, query) = build_explain_fixture().await;
        let cap = PhaseSet::default();
        let sub = Registry::default().with(cap.clone());
        with_default(sub, || {
            futures::executor::block_on(async {
                explain_change(storage.as_ref(), blamer.as_ref(), &repo_id, &query)
                    .await
                    .unwrap();
            })
        });
        let seen = cap.0.lock().unwrap();
        for required in ["blame", "hydrate_explain"] {
            assert!(
                seen.contains(required),
                "missing phase event {required}; seen = {:?}",
                *seen
            );
        }
    }
```

If `build_explain_fixture` does not already exist in this file's
tests module, factor one out from an existing test in the file
(every existing test that hits `explain_change` must already
construct these four values; promote them into a `fn` that returns
the tuple).

- [ ] **Step 3: Run the test and confirm it fails.**

```bash
cargo test -p ohara-core --lib explain::tests::explain_change_emits_blame_and_hydrate_phases
```

Expected: FAIL — phase events do not yet fire.

- [ ] **Step 4: Wrap blame + hydration with `timed_phase`.**

In `crates/ohara-core/src/explain.rs`:

Add to the imports near the top:

```rust
use crate::perf_trace::timed_phase;
```

Locate the call into the blamer (it is the line that invokes
`blamer.blame(...)` or similar inside `explain_change`). Wrap it:

```rust
let blame_lines = timed_phase("blame", async {
    blamer.blame(&query.file, query.line_start, query.line_end)
}).await?;
```

(Use the actual blamer call signature from the surrounding code; the
`timed_phase` wrapper takes any future, including ones produced by
`async {}` around a synchronous call.)

Locate the section that hydrates blame results into `ExplainHit`s
(it joins blame SHAs to commit metadata + diff hunks via the
`storage` argument). Wrap that section:

```rust
let (hits, meta) = timed_phase("hydrate_explain", async {
    // existing hydration body returns Result<(Vec<ExplainHit>, ExplainMeta), OhraError>
    /* ... */
}).await?;
```

The closure body is whatever the current hydration block does; this
task changes nothing about the logic, only the wrapper.

- [ ] **Step 5: Run the test and verify it passes.**

```bash
cargo test -p ohara-core --lib explain::tests::explain_change_emits_blame_and_hydrate_phases
```

Expected: PASS.

- [ ] **Step 6: Run the full explain test module.**

```bash
cargo test -p ohara-core --lib explain::
```

Expected: all PASS.

- [ ] **Step 7: Commit.**

```bash
git add crates/ohara-core/src/explain.rs
git commit -m "feat(core): wrap explain blame and hydration with timed_phase"
```

---

### Task A.4 — Time CLI cold-start and MCP server boot

**Files:**
- Modify: `crates/ohara-cli/src/commands/query.rs:33-69`
- Modify: `crates/ohara-cli/src/commands/explain.rs:36-58`
- Modify: `crates/ohara-mcp/src/server.rs:24-55`

- [ ] **Step 1: Wrap storage open and embedder/reranker loads in `query.rs`.**

Replace the body of `pub async fn run(args: Args) -> Result<()>` in
`crates/ohara-cli/src/commands/query.rs` so that the existing
sequential setup is wrapped with `timed_phase`. Imports near the top:

```rust
use ohara_core::perf_trace::timed_phase;
```

Body change — wrap the three setup steps and the call into the
retriever:

```rust
pub async fn run(args: Args) -> Result<()> {
    let (repo_id, _, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;

    let storage = Arc::new(
        timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?,
    );
    let chosen_provider = resolve_provider(args.embed_provider);
    tracing::info!(provider = ?chosen_provider, "embedder");

    let embedder = Arc::new(
        timed_phase("embed_load", async {
            tokio::task::spawn_blocking(move || {
                ohara_embed::FastEmbedProvider::with_provider(chosen_provider)
            })
            .await?
        })
        .await?,
    );

    let retriever = if args.no_rerank {
        Retriever::new(storage, embedder)
    } else {
        let reranker = Arc::new(
            timed_phase("rerank_load", async {
                tokio::task::spawn_blocking(move || {
                    ohara_embed::FastEmbedReranker::with_provider(chosen_provider)
                })
                .await?
            })
            .await?,
        );
        Retriever::new(storage, embedder).with_reranker(reranker)
    };

    let q = PatternQuery {
        query: args.query,
        k: args.k,
        language: args.language,
        since_unix: None,
        no_rerank: args.no_rerank,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await?;
    println!("{}", serde_json::to_string_pretty(&hits)?);
    Ok(())
}
```

Note: `chosen_provider` is `Copy` (it's an enum), so the two
`spawn_blocking` closures can each capture it independently; if the
compiler complains, change the second use to `let chosen_provider2 =
chosen_provider;` and capture `chosen_provider2`.

- [ ] **Step 2: Wrap storage open and blamer open in `explain.rs`.**

In `crates/ohara-cli/src/commands/explain.rs`, add the same import:

```rust
use ohara_core::perf_trace::timed_phase;
```

Replace the storage and blamer construction lines (currently lines
38–40):

```rust
    let storage = Arc::new(
        timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?,
    );
    let blamer = timed_phase("blamer_open", async { Blamer::open(&canonical) }).await?;
```

- [ ] **Step 3: Wrap the same three setup phases in MCP server boot.**

In `crates/ohara-mcp/src/server.rs`, add the import:

```rust
use ohara_core::perf_trace::timed_phase;
```

Inside `OharaServer::open`, replace the body (lines 24–55) with the
same three wraps:

```rust
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        let walker = ohara_git::GitWalker::open(&canonical).context("open repo")?;
        let first_commit = walker.first_commit_sha()?;
        let repo_id = RepoId::from_parts(&first_commit, &canonical.to_string_lossy());
        let db_path = ohara_core::paths::index_db_path(&repo_id)?;

        let storage: Arc<dyn Storage> = Arc::new(
            timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?,
        );

        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(
            timed_phase("embed_load", async {
                tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new).await?
            })
            .await?,
        );

        let reranker: Arc<dyn RerankProvider> = Arc::new(
            timed_phase("rerank_load", async {
                tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new).await?
            })
            .await?,
        );

        let retriever = Retriever::new(storage.clone(), embedder.clone()).with_reranker(reranker);
        let blamer = Arc::new(
            timed_phase("blamer_open", async { Blamer::open(&canonical) }).await?,
        );

        Ok(Self {
            repo_id,
            repo_path: canonical,
            storage,
            retriever,
            blamer,
        })
    }
```

- [ ] **Step 4: Build the workspace to confirm it still compiles.**

```bash
cargo build --workspace
```

Expected: success with no warnings about unused imports.

- [ ] **Step 5: Run the MCP integration tests if present, plus the CLI subcommand tests.**

```bash
cargo test -p ohara-cli
cargo test -p ohara-mcp
```

Expected: PASS. (No behavioral change; the wraps only emit events.)

- [ ] **Step 6: Commit.**

```bash
git add crates/ohara-cli/src/commands/query.rs \
        crates/ohara-cli/src/commands/explain.rs \
        crates/ohara-mcp/src/server.rs
git commit -m "feat(cli,mcp): time storage_open / embed_load / rerank_load / blamer_open"
```

---

## Phase B — Storage metrics

### Task B.1 — Add `metrics_snapshot` to the `Storage` trait

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`

- [ ] **Step 1: Add the snapshot type and trait method.**

In `crates/ohara-core/src/storage.rs`, near the top (after the existing
type aliases `pub type Vector` / `pub type HunkId`), add:

```rust
/// Per-method counters surfaced by [`Storage::metrics_snapshot`].
/// Read-only snapshot — implementations atomically copy their internal
/// counters into this shape so callers see a consistent view.
#[derive(Debug, Default, Clone)]
pub struct StorageMethodMetrics {
    pub call_count: u64,
    pub total_elapsed_us: u64,
    pub rows_returned: u64,
}

#[derive(Debug, Default, Clone)]
pub struct StorageMetricsSnapshot {
    pub knn_hunks: StorageMethodMetrics,
    pub bm25_hunks_by_text: StorageMethodMetrics,
    pub bm25_hunks_by_semantic_text: StorageMethodMetrics,
    pub bm25_hunks_by_symbol_name: StorageMethodMetrics,
    pub bm25_hunks_by_historical_symbol: StorageMethodMetrics,
    pub get_hunk_symbols: StorageMethodMetrics,
    pub get_neighboring_file_commits: StorageMethodMetrics,
    pub get_index_status: StorageMethodMetrics,
    pub get_index_metadata: StorageMethodMetrics,
}
```

Then, at the bottom of the `pub trait Storage` body (just before the
closing `}`), add:

```rust
    /// Snapshot of per-method counters since process start. Default
    /// implementation returns zeros — only `SqliteStorage` ships with
    /// real numbers; test fakes that don't override it appear silent
    /// in the harness, which is the right default.
    fn metrics_snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot::default()
    }
```

- [ ] **Step 2: Build to confirm trait compiles and existing impls still satisfy it.**

```bash
cargo build -p ohara-core -p ohara-storage
```

Expected: success. The default body means existing `Storage`
implementations don't need changes yet.

- [ ] **Step 3: Commit.**

```bash
git add crates/ohara-core/src/storage.rs
git commit -m "feat(core): add Storage::metrics_snapshot with default impl"
```

---

### Task B.2 — Wire `AtomicU64` counters into `SqliteStorage`

**Files:**
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [ ] **Step 1: Write a failing test.**

Append to the existing test module at the bottom of
`crates/ohara-storage/src/storage_impl.rs` (or create one if absent;
follow the pattern of any existing in-file test; otherwise add at the
end of the file):

```rust
#[cfg(test)]
mod metrics_tests {
    use super::*;
    use ohara_core::storage::Storage;
    use ohara_core::types::RepoId;

    #[tokio::test]
    async fn knn_call_increments_counters() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("idx.sqlite"))
            .await
            .unwrap();
        let repo_id = RepoId::from_parts("seed", "/p");
        s.open_repo(&repo_id, "/p", "seed").await.unwrap();

        let before = s.metrics_snapshot().knn_hunks.call_count;
        // KNN against an empty index returns Ok(vec![]); we don't care
        // about the result, only the counter side-effect.
        let _ = s
            .knn_hunks(&repo_id, &vec![0.0_f32; 384], 5, None, None)
            .await;
        let after = s.metrics_snapshot().knn_hunks.call_count;
        assert_eq!(after, before + 1, "knn_hunks counter should increment by 1");

        let elapsed = s.metrics_snapshot().knn_hunks.total_elapsed_us;
        assert!(elapsed > 0, "elapsed_us should be non-zero after a call");
    }
}
```

- [ ] **Step 2: Run the test and confirm it fails.**

```bash
cargo test -p ohara-storage --lib storage_impl::metrics_tests::knn_call_increments_counters
```

Expected: FAIL — counters not yet implemented (the default-impl
snapshot always returns 0, so `after` stays at `before`).

- [ ] **Step 3: Add the counters struct.**

At the top of `crates/ohara-storage/src/storage_impl.rs`, after the
existing `use` block, add:

```rust
use ohara_core::storage::{StorageMethodMetrics, StorageMetricsSnapshot};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
struct MethodCounter {
    call_count: AtomicU64,
    total_elapsed_us: AtomicU64,
    rows_returned: AtomicU64,
}

impl MethodCounter {
    fn record(&self, elapsed_us: u64, rows: u64) {
        self.call_count.fetch_add(1, Relaxed);
        self.total_elapsed_us.fetch_add(elapsed_us, Relaxed);
        self.rows_returned.fetch_add(rows, Relaxed);
    }
    fn snapshot(&self) -> StorageMethodMetrics {
        StorageMethodMetrics {
            call_count: self.call_count.load(Relaxed),
            total_elapsed_us: self.total_elapsed_us.load(Relaxed),
            rows_returned: self.rows_returned.load(Relaxed),
        }
    }
}

#[derive(Default)]
struct StorageCounters {
    knn_hunks: MethodCounter,
    bm25_hunks_by_text: MethodCounter,
    bm25_hunks_by_semantic_text: MethodCounter,
    bm25_hunks_by_symbol_name: MethodCounter,
    bm25_hunks_by_historical_symbol: MethodCounter,
    get_hunk_symbols: MethodCounter,
    get_neighboring_file_commits: MethodCounter,
    get_index_status: MethodCounter,
    get_index_metadata: MethodCounter,
}

impl StorageCounters {
    fn snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot {
            knn_hunks: self.knn_hunks.snapshot(),
            bm25_hunks_by_text: self.bm25_hunks_by_text.snapshot(),
            bm25_hunks_by_semantic_text: self.bm25_hunks_by_semantic_text.snapshot(),
            bm25_hunks_by_symbol_name: self.bm25_hunks_by_symbol_name.snapshot(),
            bm25_hunks_by_historical_symbol: self.bm25_hunks_by_historical_symbol.snapshot(),
            get_hunk_symbols: self.get_hunk_symbols.snapshot(),
            get_neighboring_file_commits: self.get_neighboring_file_commits.snapshot(),
            get_index_status: self.get_index_status.snapshot(),
            get_index_metadata: self.get_index_metadata.snapshot(),
        }
    }
}
```

Update the struct definition:

```rust
pub struct SqliteStorage {
    pool: Pool,
    counters: StorageCounters,
}
```

Update `SqliteStorage::open` to initialize counters:

```rust
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let pool = SqlitePoolBuilder::new(path).build().await?;
        let conn = pool.get().await?;
        conn.interact(migrations::run)
            .await
            .map_err(|e| anyhow::anyhow!("interact: {e}"))??;
        Ok(Self {
            pool,
            counters: StorageCounters::default(),
        })
    }
```

- [ ] **Step 4: Add a small recording helper next to `with_conn`.**

Right after the existing `async fn with_conn<F, T>` definition, add:

```rust
async fn timed_with_conn<F, T>(
    pool: &deadpool_sqlite::Pool,
    counter: &MethodCounter,
    rows_of: impl Fn(&T) -> u64,
    f: F,
) -> ohara_core::Result<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let start = std::time::Instant::now();
    let out = with_conn(pool, f).await?;
    let elapsed_us = start.elapsed().as_micros() as u64;
    let rows = rows_of(&out);
    counter.record(elapsed_us, rows);
    Ok(out)
}
```

- [ ] **Step 5: Replace `with_conn` with `timed_with_conn` in the nine
counted methods.**

For each of these nine `Storage` impl methods on `SqliteStorage`,
replace the `with_conn` call with `timed_with_conn`, threading in
the matching counter and an appropriate `rows_of` closure:

| Method | Counter field | `rows_of` |
|---|---|---|
| `knn_hunks` | `self.counters.knn_hunks` | `|v: &Vec<HunkHit>| v.len() as u64` |
| `bm25_hunks_by_text` | `self.counters.bm25_hunks_by_text` | `|v: &Vec<HunkHit>| v.len() as u64` |
| `bm25_hunks_by_semantic_text` | `self.counters.bm25_hunks_by_semantic_text` | `|v| v.len() as u64` |
| `bm25_hunks_by_symbol_name` | `self.counters.bm25_hunks_by_symbol_name` | `|v| v.len() as u64` |
| `bm25_hunks_by_historical_symbol` | `self.counters.bm25_hunks_by_historical_symbol` | `|v| v.len() as u64` |
| `get_hunk_symbols` | `self.counters.get_hunk_symbols` | `|v: &Vec<HunkSymbol>| v.len() as u64` |
| `get_neighboring_file_commits` | `self.counters.get_neighboring_file_commits` | `|v: &Vec<(u32, CommitMeta)>| v.len() as u64` |
| `get_index_status` | `self.counters.get_index_status` | `|_: &IndexStatus| 1` |
| `get_index_metadata` | `self.counters.get_index_metadata` | `|m: &StoredIndexMetadata| m.len() as u64` |

For `get_index_metadata`, if `StoredIndexMetadata` doesn't expose a
`len()`, count rows however its public surface allows (e.g., the
number of populated `Option` fields, or just `1` — the field exists
to flag that the call happened, not to be precise).

Concrete example — replace the existing `knn_hunks` body:

```rust
    async fn knn_hunks(
        &self,
        repo_id: &RepoId,
        query_emb: &[f32],
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let id = repo_id.as_str().to_string();
        let qe = query_emb.to_vec();
        let lang = language.map(|s| s.to_string());
        timed_with_conn(
            &self.pool,
            &self.counters.knn_hunks,
            |v: &Vec<HunkHit>| v.len() as u64,
            move |c| crate::tables::hunk::knn(c, &id, &qe, k, lang.as_deref(), since_unix),
        )
        .await
    }
```

Apply the equivalent change to the other eight methods listed in the
table.

- [ ] **Step 6: Implement `metrics_snapshot` on `SqliteStorage`.**

In the `impl Storage for SqliteStorage` block, add at the bottom
(overriding the trait default):

```rust
    fn metrics_snapshot(&self) -> StorageMetricsSnapshot {
        self.counters.snapshot()
    }
```

- [ ] **Step 7: Run the test and verify it passes.**

```bash
cargo test -p ohara-storage --lib storage_impl::metrics_tests::knn_call_increments_counters
```

Expected: PASS.

- [ ] **Step 8: Run the full storage test suite to ensure no regressions.**

```bash
cargo test -p ohara-storage
```

Expected: all PASS.

- [ ] **Step 9: Commit.**

```bash
git add crates/ohara-storage/src/storage_impl.rs
git commit -m "feat(storage): per-method atomic counters and metrics_snapshot"
```

---

### Task B.3 — Opt-in SQL trace via `Connection::trace`

**Files:**
- Modify: `crates/ohara-storage/src/codec/pool.rs`

- [ ] **Step 1: Read the current pool builder.**

Open `crates/ohara-storage/src/codec/pool.rs` and identify the
function that opens each `rusqlite::Connection` (most likely a
closure passed to `deadpool_sqlite`'s `Manager`).

- [ ] **Step 2: Write a failing test.**

Append to the same file (or create a `#[cfg(test)] mod tests { ... }`
block if missing):

```rust
#[cfg(test)]
mod sql_trace_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    #[derive(Default, Clone)]
    struct SqlEvents(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SqlEvents {
        fn on_event(
            &self,
            ev: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if ev.metadata().target() != "ohara_storage::sql" {
                return;
            }
            struct V<'a>(&'a mut String);
            impl<'a> tracing::field::Visit for V<'a> {
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    if f.name() == "sql" {
                        *self.0 = v.to_string();
                    }
                }
                fn record_u64(&mut self, _: &tracing::field::Field, _: u64) {}
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut sql = String::new();
            ev.record(&mut V(&mut sql));
            self.0.lock().unwrap().push(sql);
        }
    }

    #[tokio::test]
    async fn sql_trace_emits_events_when_target_is_enabled() {
        // Force the trace target on for this test even if the
        // surrounding env doesn't set RUST_LOG. The pool installs the
        // callback unconditionally; the runtime cost is gated by
        // tracing's level filter on the subscriber side, which the
        // production CLI configures via env-filter.
        let cap = SqlEvents::default();
        let sub = Registry::default().with(cap.clone());
        with_default(sub, || {
            futures::executor::block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let pool = SqlitePoolBuilder::new(dir.path().join("t.sqlite"))
                    .build()
                    .await
                    .unwrap();
                let conn = pool.get().await.unwrap();
                conn.interact(|c| {
                    c.execute_batch("CREATE TABLE probe (id INTEGER); SELECT 1;")
                })
                .await
                .unwrap()
                .unwrap();
            });
        });
        let events = cap.0.lock().unwrap();
        assert!(
            events.iter().any(|s| s.contains("CREATE TABLE probe")),
            "expected SQL trace event for CREATE TABLE; got {:?}",
            *events
        );
    }
}
```

- [ ] **Step 3: Confirm it fails.**

```bash
cargo test -p ohara-storage --lib codec::pool::sql_trace_tests::sql_trace_emits_events_when_target_is_enabled
```

Expected: FAIL — no trace callback installed yet.

- [ ] **Step 4: Install the trace callback.**

In the connection-init closure inside `SqlitePoolBuilder::build`,
register a trace callback. The exact insertion point is the closure
that receives a `rusqlite::Connection` after `Manager` opens it; if
the file uses `deadpool_sqlite::Manager::from_config` or similar,
add the install via `Pool::interact` immediately after `build()`, or
hook it into the per-connection setup via the manager's
`recycle`/`create` hooks. Concretely, add:

```rust
fn install_sql_trace(conn: &rusqlite::Connection) {
    // Always install. tracing's subscriber-side filter decides whether
    // the event materializes — when no subscriber listens on
    // `ohara_storage::sql`, the callback's tracing::trace! is a near
    // no-op (one atomic load + early-return).
    conn.trace(Some(|sql: &str| {
        tracing::trace!(target: "ohara_storage::sql", sql);
    }));
}
```

Then call `install_sql_trace(&c)` once per opened connection. If the
existing `SqlitePoolBuilder` already exposes a per-connection setup
hook (look for a `customize` or `setup` closure on the manager),
plug it in there. Otherwise wrap the `Manager::create` future to
install the callback before returning the connection.

If `rusqlite::Connection::trace` requires an `unsafe fn` pointer in
the version pinned by the workspace, accept the function signature
the API requires (the rusqlite 0.31 docs show
`trace(Option<fn(&str)>)`, which is safe).

- [ ] **Step 5: Run the test and verify it passes.**

```bash
cargo test -p ohara-storage --lib codec::pool::sql_trace_tests::sql_trace_emits_events_when_target_is_enabled
```

Expected: PASS.

- [ ] **Step 6: Run the full storage test suite.**

```bash
cargo test -p ohara-storage
```

Expected: all PASS.

- [ ] **Step 7: Commit.**

```bash
git add crates/ohara-storage/src/codec/pool.rs
git commit -m "feat(storage): per-statement SQL trace on ohara_storage::sql target"
```

---

## Phase C — `--trace-perf` CLI flag and aggregator

### Task C.1 — Aggregating subscriber layer in `ohara-cli`

**Files:**
- Create: `crates/ohara-cli/src/perf_trace.rs`
- Modify: `crates/ohara-cli/src/main.rs`
- Modify: `crates/ohara-cli/Cargo.toml`

- [ ] **Step 1: Write the aggregator.**

Create `crates/ohara-cli/src/perf_trace.rs`:

```rust
//! `--trace-perf` plumbing — installs a `tracing-subscriber` layer
//! that captures every `ohara::phase` event, accumulates per-phase
//! totals + counts, and prints a compact summary to stderr at
//! process exit.
//!
//! End-user output shape (one line per phase, plus a `total`):
//!
//! ```text
//! [phase] storage_open    8ms    n=1
//! [phase] embed_load   1820ms    n=1   (cold)
//! [phase] embed_query    12ms    n=1
//! [phase] lane_knn       24ms    n=1   hits=87
//! [phase] total        7042ms
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing::Event;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

#[derive(Default, Clone)]
struct PhaseAcc {
    total_ms: u64,
    calls: u64,
    hits: u64,
}

#[derive(Default, Clone)]
pub struct PerfAccumulator {
    phases: Arc<Mutex<std::collections::BTreeMap<String, PhaseAcc>>>,
}

impl PerfAccumulator {
    pub fn print_summary_to_stderr(&self) {
        let phases = self.phases.lock().unwrap();
        let mut total_ms = 0_u64;
        for (name, acc) in phases.iter() {
            total_ms += acc.total_ms;
            let hits_part = if acc.hits > 0 {
                format!("   hits={}", acc.hits)
            } else {
                String::new()
            };
            eprintln!(
                "[phase] {name:<22} {ms:>5}ms   n={n}{hits}",
                name = name,
                ms = acc.total_ms,
                n = acc.calls,
                hits = hits_part,
            );
        }
        eprintln!("[phase] {:<22} {:>5}ms", "total", total_ms);
    }
}

impl<S: tracing::Subscriber> Layer<S> for PerfAccumulator {
    fn on_event(&self, ev: &Event<'_>, _: Context<'_, S>) {
        if ev.metadata().target() != "ohara::phase" {
            return;
        }
        struct V {
            phase: Option<String>,
            elapsed_ms: u64,
            hit_count: u64,
        }
        impl Visit for V {
            fn record_str(&mut self, f: &Field, v: &str) {
                if f.name() == "phase" {
                    self.phase = Some(v.to_string());
                }
            }
            fn record_u64(&mut self, f: &Field, v: u64) {
                match f.name() {
                    "elapsed_ms" => self.elapsed_ms = v,
                    "hit_count" => self.hit_count = v,
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }
        let mut v = V {
            phase: None,
            elapsed_ms: 0,
            hit_count: 0,
        };
        ev.record(&mut v);
        if let Some(name) = v.phase {
            let mut g = self.phases.lock().unwrap();
            let entry = g.entry(name).or_default();
            entry.total_ms += v.elapsed_ms;
            entry.calls += 1;
            entry.hits += v.hit_count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    #[test]
    fn aggregator_sums_two_events_for_same_phase() {
        let acc = PerfAccumulator::default();
        let sub = Registry::default().with(acc.clone());
        with_default(sub, || {
            tracing::info!(target: "ohara::phase", phase = "lane_knn", elapsed_ms = 10_u64);
            tracing::info!(target: "ohara::phase", phase = "lane_knn", elapsed_ms = 20_u64, hit_count = 5_u64);
            tracing::info!(target: "ohara::phase", phase = "rrf", elapsed_ms = 1_u64);
        });
        let phases = acc.phases.lock().unwrap();
        let knn = &phases["lane_knn"];
        assert_eq!(knn.calls, 2);
        assert_eq!(knn.total_ms, 30);
        assert_eq!(knn.hits, 5);
        let rrf = &phases["rrf"];
        assert_eq!(rrf.total_ms, 1);
    }
}
```

- [ ] **Step 2: Add `--trace-perf` global flag and install the layer.**

Open `crates/ohara-cli/src/main.rs`. Add:

```rust
mod perf_trace;
```

near the top, alongside other module declarations. Then locate the
top-level `clap` struct (the one annotated `#[derive(Parser)]`). Add a
new flag (in the same struct):

```rust
    /// Print per-phase elapsed times to stderr at process exit.
    /// Aggregates `ohara::phase` tracing events emitted by `ohara-core`
    /// and `ohara-storage`.
    #[arg(long, global = true)]
    pub trace_perf: bool,
```

In the `main` function, after the existing `tracing-subscriber` init
(or alongside it — the existing init likely uses
`tracing_subscriber::fmt::init()` or similar), conditionally compose
the perf accumulator. Replace whatever currently initializes the
subscriber with:

```rust
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

let perf_acc = if args.trace_perf {
    Some(perf_trace::PerfAccumulator::default())
} else {
    None
};

let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

let registry = tracing_subscriber::registry()
    .with(env_filter)
    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

match &perf_acc {
    Some(acc) => registry.with(acc.clone()).init(),
    None => registry.init(),
}
```

After the dispatch to the chosen subcommand returns, before `main`
exits, dump the summary:

```rust
if let Some(acc) = perf_acc {
    acc.print_summary_to_stderr();
}
```

If `main` currently uses `?`-bubbling and an early return, factor the
dispatch into a helper that returns `Result<()>` so the summary
prints regardless of success or failure. Sketch:

```rust
async fn run() -> Result<()> { /* existing dispatch */ }

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    /* ...subscriber init as above... */
    let outcome = run(args).await;
    if let Some(acc) = perf_acc { acc.print_summary_to_stderr(); }
    outcome
}
```

- [ ] **Step 3: Update Cargo.toml dev-deps if needed.**

`tracing-subscriber` is already a workspace dep (see root
`Cargo.toml`). Make sure `crates/ohara-cli/Cargo.toml` has:

```toml
[dependencies]
tracing.workspace = true
tracing-subscriber.workspace = true
```

If `tracing-subscriber` is currently absent from the cli's
`[dependencies]`, add it.

- [ ] **Step 4: Run the unit test.**

```bash
cargo test -p ohara-cli --lib perf_trace::tests::aggregator_sums_two_events_for_same_phase
```

Expected: PASS.

- [ ] **Step 5: Smoke-test against the tiny fixture.**

```bash
fixtures/build_tiny.sh
cargo run --release -p ohara-cli -- --trace-perf index fixtures/tiny/repo
cargo run --release -p ohara-cli -- --trace-perf query fixtures/tiny/repo --query "retry"
```

Expected: the second invocation prints `[phase]` lines on stderr
ending in `[phase] total <N>ms`. Numbers will vary.

- [ ] **Step 6: Commit.**

```bash
git add crates/ohara-cli/src/perf_trace.rs \
        crates/ohara-cli/src/main.rs \
        crates/ohara-cli/Cargo.toml
git commit -m "feat(cli): --trace-perf flag and per-phase aggregator"
```

---

## Phase D — Medium fixture (ripgrep)

### Task D.1 — `fixtures/build_medium.sh`

**Files:**
- Create: `fixtures/build_medium.sh`
- Modify: `.gitignore`

- [ ] **Step 1: Add the build script.**

Create `fixtures/build_medium.sh`:

```bash
#!/usr/bin/env bash
# Builds fixtures/medium/repo: a shallow clone of ripgrep at tag 14.1.1
# used by the perf harness binaries (cli_query_bench, mcp_query_bench).
#
# Idempotent: re-running re-uses the existing checkout. The first
# successful clone records the resolved tag SHA into
# fixtures/medium/.fixture-sha; subsequent runs assert it matches so
# upstream tag re-points are caught by the harness rather than
# silently shifting numbers.
#
# Run:
#   fixtures/build_medium.sh
#
# Wipe and re-clone:
#   rm -rf fixtures/medium && fixtures/build_medium.sh

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HERE/medium/repo"
SHA_FILE="$HERE/medium/.fixture-sha"
TAG="${OHARA_RIPGREP_TAG:-14.1.1}"
URL="${OHARA_RIPGREP_URL:-https://github.com/BurntSushi/ripgrep.git}"

mkdir -p "$HERE/medium"

if [ ! -d "$DEST/.git" ]; then
    echo "[medium] cloning $URL @ $TAG"
    # Shallow clone; depth 5000 covers the full ripgrep history at
    # 14.1.1 (~3500 commits) plus headroom for future tag bumps.
    git clone --depth 5000 --branch "$TAG" "$URL" "$DEST"
fi

resolved="$(git -C "$DEST" rev-parse HEAD)"

if [ -f "$SHA_FILE" ]; then
    expected="$(cat "$SHA_FILE")"
    if [ "$resolved" != "$expected" ]; then
        echo "[medium] fixture SHA drift: expected $expected, got $resolved" >&2
        echo "[medium] either upstream re-pointed $TAG or your checkout is stale." >&2
        echo "[medium] inspect with: git -C $DEST log -1 --oneline" >&2
        exit 1
    fi
else
    echo "[medium] recording fixture SHA: $resolved"
    echo "$resolved" > "$SHA_FILE"
fi

echo "[medium] ready: $DEST @ $resolved"
```

- [ ] **Step 2: Make it executable.**

```bash
chmod +x fixtures/build_medium.sh
```

- [ ] **Step 3: Add to `.gitignore`.**

Append to `.gitignore`:

```
fixtures/medium/repo/
```

(Leave `.fixture-sha` checked in — it's the contract that pins the
clone hash.)

- [ ] **Step 4: Run it once to capture the SHA.**

```bash
fixtures/build_medium.sh
```

Expected: clones into `fixtures/medium/repo`, prints
`[medium] recording fixture SHA: <SHA>`. The new file
`fixtures/medium/.fixture-sha` should be created.

- [ ] **Step 5: Run it again to verify idempotency.**

```bash
fixtures/build_medium.sh
```

Expected: prints `[medium] ready: …` without re-cloning, no SHA
mismatch error.

- [ ] **Step 6: Commit (script + .fixture-sha + gitignore).**

```bash
git add fixtures/build_medium.sh fixtures/medium/.fixture-sha .gitignore
git commit -m "feat(fixtures): build_medium.sh — ripgrep 14.1.1 perf fixture"
```

---

## Phase E — Harness binaries

### Task E.1 — `cli_query_bench` test-binary

**Files:**
- Create: `tests/perf/cli_query_bench.rs`
- Modify: `tests/perf/Cargo.toml`

- [ ] **Step 1: Add the `[[test]]` entry.**

Append to `tests/perf/Cargo.toml`:

```toml
[[test]]
# Plan 14 Task E.1 — CLI cold-path perf harness. `#[ignore]`'d; opt
# in via `cargo test -p ohara-perf-tests --release -- --ignored
# cli_query_bench --nocapture`.
name = "cli_query_bench"
path = "cli_query_bench.rs"
```

- [ ] **Step 2: Create the harness file.**

Create `tests/perf/cli_query_bench.rs`:

```rust
//! Plan 14 Task E.1 — CLI cold-path perf harness.
//!
//! Runs `ohara query --trace-perf` N times against the medium ripgrep
//! fixture and writes per-phase histograms to
//! `target/perf/runs/<git_sha>-<utc>.json`. Designed to be operator-run
//! (`#[ignore]`'d) — the spec calls for harness numbers in PR
//! descriptions, not CI gates.
//!
//! Run:
//! ```sh
//! fixtures/build_medium.sh
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored cli_query_bench --nocapture
//! ```

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const ITERATIONS: usize = 5;
const QUERY: &str = "retry with backoff";

#[derive(Debug, Default, Serialize, Clone)]
struct PhaseStats {
    samples: Vec<u64>, // ms
}

impl PhaseStats {
    fn p50(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        s[s.len() / 2]
    }
    fn min(&self) -> u64 {
        *self.samples.iter().min().unwrap_or(&0)
    }
    fn max(&self) -> u64 {
        *self.samples.iter().max().unwrap_or(&0)
    }
}

#[derive(Debug, Serialize)]
struct RunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    query: String,
    wall_ms: PhaseStats,
    phases: BTreeMap<String, PhaseStats>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn ensure_medium_fixture() -> PathBuf {
    let root = workspace_root();
    let script = root.join("fixtures/build_medium.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("run build_medium.sh");
    assert!(status.success(), "build_medium.sh failed");
    let dest = root.join("fixtures/medium/repo");
    assert!(dest.join(".git").is_dir(), "medium fixture not present");
    dest
}

fn current_git_sha(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Parse one `[phase] <name>     <N>ms ...` line from `--trace-perf`
/// stderr. Returns `(name, ms)` or `None` when the line is the trailing
/// `total` summary or any unrelated stderr noise.
fn parse_phase_line(line: &str) -> Option<(String, u64)> {
    let l = line.trim_start();
    let rest = l.strip_prefix("[phase] ")?;
    let mut it = rest.split_whitespace();
    let name = it.next()?.to_string();
    if name == "total" {
        return None;
    }
    let ms_token = it.next()?;
    let ms_str = ms_token.strip_suffix("ms")?;
    let ms: u64 = ms_str.parse().ok()?;
    Some((name, ms))
}

fn write_report(report: &RunReport) -> PathBuf {
    let root = workspace_root();
    let dir = root.join("target/perf/runs");
    std::fs::create_dir_all(&dir).expect("mkdir target/perf/runs");
    let path = dir.join(format!("{}-{}-cli-query.json", report.git_sha, report.utc));
    let json = serde_json::to_string_pretty(report).expect("serialize report");
    std::fs::write(&path, json).expect("write report");
    path
}

#[test]
#[ignore = "perf harness — opt in via --ignored"]
fn cli_query_bench_emits_run_report() {
    let fixture = ensure_medium_fixture();
    let root = workspace_root();
    let bin = root.join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    let mut wall = PhaseStats::default();
    let mut phases: BTreeMap<String, PhaseStats> = BTreeMap::new();

    for i in 0..ITERATIONS {
        let start = Instant::now();
        let out = Command::new(&bin)
            .arg("--trace-perf")
            .arg("query")
            .arg(&fixture)
            .arg("--query")
            .arg(QUERY)
            .arg("--no-rerank") // exclude rerank cold-load on the cold-CLI path
            .output()
            .expect("spawn ohara");
        let elapsed = start.elapsed().as_millis() as u64;
        wall.samples.push(elapsed);
        if !out.status.success() {
            panic!(
                "iter {i}: ohara query failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            if let Some((name, ms)) = parse_phase_line(line) {
                phases.entry(name).or_default().samples.push(ms);
            }
        }
    }

    let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let report = RunReport {
        git_sha: current_git_sha(&root),
        utc,
        iterations: ITERATIONS,
        fixture: fixture.display().to_string(),
        query: QUERY.to_string(),
        wall_ms: wall,
        phases,
    };
    let path = write_report(&report);
    eprintln!("wrote {}", path.display());
    eprintln!(
        "wall p50={}ms min={}ms max={}ms",
        report.wall_ms.p50(),
        report.wall_ms.min(),
        report.wall_ms.max()
    );
    for (name, stats) in &report.phases {
        eprintln!(
            "phase {name:<22} p50={:>5}ms min={:>5}ms max={:>5}ms n={}",
            stats.p50(),
            stats.min(),
            stats.max(),
            stats.samples.len()
        );
    }
}
```

- [ ] **Step 3: Build the release binary needed by the harness.**

```bash
cargo build --release -p ohara-cli
```

Expected: success.

- [ ] **Step 4: Run the harness.**

```bash
cargo test -p ohara-perf-tests --release -- --ignored cli_query_bench --nocapture
```

Expected: PASS. A JSON file appears under `target/perf/runs/`.

- [ ] **Step 5: Commit.**

```bash
git add tests/perf/cli_query_bench.rs tests/perf/Cargo.toml
git commit -m "test(perf): cli_query_bench harness binary"
```

---

### Task E.2 — `mcp_query_bench` test-binary

**Files:**
- Create: `tests/perf/mcp_query_bench.rs`
- Modify: `tests/perf/Cargo.toml`

- [ ] **Step 1: Register the binary.**

Append to `tests/perf/Cargo.toml`:

```toml
[[test]]
# Plan 14 Task E.2 — MCP in-process perf harness. Drives
# OharaService::find_pattern + explain_change directly without rmcp
# framing so we measure server-side latency only.
name = "mcp_query_bench"
path = "mcp_query_bench.rs"
```

If `ohara-mcp` is not yet listed in `[dev-dependencies]`, add:

```toml
ohara-mcp = { path = "../../crates/ohara-mcp" }
```

- [ ] **Step 2: Create the harness.**

Create `tests/perf/mcp_query_bench.rs`:

```rust
//! Plan 14 Task E.2 — in-process MCP harness. Constructs an
//! `OharaServer` against the medium ripgrep fixture, then drives
//! `find_pattern` and `explain_change` repeatedly. Numbers reflect
//! the **warm** path — cold-load happens once at server boot and is
//! reported as its own phase.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

const ITERATIONS: usize = 10;
const QUERY: &str = "retry with backoff";
const EXPLAIN_FILE: &str = "src/main.rs";
const EXPLAIN_LINES: (u32, u32) = (1, 50);

#[derive(Debug, Default, Serialize, Clone)]
struct PhaseStats {
    samples: Vec<u64>,
}
impl PhaseStats {
    fn p50(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        s[s.len() / 2]
    }
}

#[derive(Debug, Serialize)]
struct McpRunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    boot_ms: u64,
    find_pattern_wall_ms: PhaseStats,
    explain_change_wall_ms: PhaseStats,
    phases: BTreeMap<String, PhaseStats>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn ensure_medium_fixture() -> PathBuf {
    let root = workspace_root();
    let script = root.join("fixtures/build_medium.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("run build_medium.sh");
    assert!(status.success(), "build_medium.sh failed");
    root.join("fixtures/medium/repo")
}

fn current_git_sha(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf harness — opt in via --ignored"]
async fn mcp_query_bench_emits_run_report() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Instant;
    use tracing::field::{Field, Visit};
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;
    use tracing_subscriber::Registry;

    // The server boot path indexes nothing — it just opens the
    // existing index for the fixture. Operators are expected to have
    // already run `ohara index` against fixtures/medium/repo at least
    // once before invoking this harness; the assertion below catches
    // the omission with an actionable message.
    let fixture = ensure_medium_fixture();
    let root = workspace_root();

    // Re-use the harness phase target by installing the same layer
    // shape used by the CLI accumulator, but accumulating into a
    // local map so this test owns its data.
    #[derive(Default, Clone)]
    struct PhaseAcc(Arc<Mutex<BTreeMap<String, PhaseStats>>>);
    impl<S: tracing::Subscriber> Layer<S> for PhaseAcc {
        fn on_event(&self, ev: &tracing::Event<'_>, _: Context<'_, S>) {
            if ev.metadata().target() != "ohara::phase" {
                return;
            }
            struct V {
                name: Option<String>,
                ms: u64,
            }
            impl Visit for V {
                fn record_str(&mut self, f: &Field, v: &str) {
                    if f.name() == "phase" {
                        self.name = Some(v.to_string());
                    }
                }
                fn record_u64(&mut self, f: &Field, v: u64) {
                    if f.name() == "elapsed_ms" {
                        self.ms = v;
                    }
                }
                fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
            }
            let mut v = V { name: None, ms: 0 };
            ev.record(&mut v);
            if let Some(n) = v.name {
                let mut g = self.0.lock().unwrap();
                g.entry(n).or_default().samples.push(v.ms);
            }
        }
    }

    let acc = PhaseAcc::default();
    let sub = Registry::default().with(acc.clone());

    let mut find_wall = PhaseStats::default();
    let mut explain_wall = PhaseStats::default();
    let boot_start = Instant::now();
    let report_path = with_default(sub, || {
        futures::executor::block_on(async {
            let server = ohara_mcp::server::OharaServer::open(&fixture)
                .await
                .expect("OharaServer::open against medium fixture (run `ohara index fixtures/medium/repo` first)");
            let boot_ms = boot_start.elapsed().as_millis() as u64;
            let service = ohara_mcp::tools::find_pattern::OharaService::new(server);

            for _ in 0..ITERATIONS {
                let req_start = Instant::now();
                let _out = service
                    .find_pattern(ohara_mcp::tools::find_pattern::FindPatternInput {
                        query: QUERY.to_string(),
                        k: 5,
                        language: None,
                        since: None,
                        no_rerank: false,
                    })
                    .await
                    .expect("find_pattern");
                find_wall.samples.push(req_start.elapsed().as_millis() as u64);
            }

            for _ in 0..ITERATIONS {
                let req_start = Instant::now();
                let _out = service
                    .explain_change(ohara_mcp::tools::explain_change::ExplainChangeInput {
                        file: EXPLAIN_FILE.to_string(),
                        line_start: EXPLAIN_LINES.0,
                        line_end: EXPLAIN_LINES.1,
                        k: 5,
                        include_diff: true,
                    })
                    .await
                    .expect("explain_change");
                explain_wall.samples.push(req_start.elapsed().as_millis() as u64);
            }

            let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let phases = acc.0.lock().unwrap().clone();
            let report = McpRunReport {
                git_sha: current_git_sha(&root),
                utc,
                iterations: ITERATIONS,
                fixture: fixture.display().to_string(),
                boot_ms,
                find_pattern_wall_ms: find_wall.clone(),
                explain_change_wall_ms: explain_wall.clone(),
                phases,
            };
            let dir = root.join("target/perf/runs");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{}-{}-mcp.json", report.git_sha, report.utc));
            std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
            path
        })
    });

    eprintln!(
        "wrote {} (find_pattern p50={}ms, explain_change p50={}ms)",
        report_path.display(),
        find_wall.p50(),
        explain_wall.p50()
    );
}
```

- [ ] **Step 3: Pre-index the medium fixture.**

```bash
fixtures/build_medium.sh
cargo run --release -p ohara-cli -- index fixtures/medium/repo
```

Expected: index completes (will take a while — this is the index
path, intentionally not optimized in this plan).

- [ ] **Step 4: Run the harness.**

```bash
cargo test -p ohara-perf-tests --release -- --ignored mcp_query_bench --nocapture
```

Expected: PASS. A `*-mcp.json` report appears under
`target/perf/runs/`.

- [ ] **Step 5: Commit.**

```bash
git add tests/perf/mcp_query_bench.rs tests/perf/Cargo.toml
git commit -m "test(perf): mcp_query_bench harness binary"
```

---

### Task E.3 — `perf_diff` summary tool

**Files:**
- Create: `tests/perf/perf_diff.rs`
- Modify: `tests/perf/Cargo.toml`

- [ ] **Step 1: Register the binary.**

Append to `tests/perf/Cargo.toml`:

```toml
[[test]]
# Plan 14 Task E.3 — perf-run differ. Reads two JSON reports written
# by cli_query_bench / mcp_query_bench and prints a side-by-side
# delta. Operators paste the output into PR descriptions.
name = "perf_diff"
path = "perf_diff.rs"
```

- [ ] **Step 2: Create the differ.**

Create `tests/perf/perf_diff.rs`:

```rust
//! Plan 14 Task E.3 — diff two perf runs.
//!
//! `OHARA_PERF_DIFF_BEFORE` and `OHARA_PERF_DIFF_AFTER` env vars point
//! at two JSON reports produced by `cli_query_bench` / `mcp_query_bench`.
//! The test prints a per-phase delta to stderr.
//!
//! Run:
//! ```sh
//! cargo test -p ohara-perf-tests --release -- --ignored perf_diff --nocapture \
//!     OHARA_PERF_DIFF_BEFORE=target/perf/runs/...-cli-query.json \
//!     OHARA_PERF_DIFF_AFTER=target/perf/runs/...-cli-query.json
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct PhaseStats {
    samples: Vec<u64>,
}
impl PhaseStats {
    fn p50(&self) -> i64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        s[s.len() / 2] as i64
    }
}

#[derive(Debug, Deserialize)]
struct RunReportLite {
    #[serde(default)]
    wall_ms: Option<PhaseStats>,
    #[serde(default)]
    find_pattern_wall_ms: Option<PhaseStats>,
    #[serde(default)]
    explain_change_wall_ms: Option<PhaseStats>,
    phases: BTreeMap<String, PhaseStats>,
}

fn read(path: &Path) -> RunReportLite {
    let s = std::fs::read_to_string(path).expect("read perf report");
    serde_json::from_str(&s).expect("parse perf report")
}

#[test]
#[ignore = "perf-diff utility — opt in via --ignored"]
fn perf_diff_prints_per_phase_delta() {
    let before_path = std::env::var("OHARA_PERF_DIFF_BEFORE")
        .expect("set OHARA_PERF_DIFF_BEFORE=<path-to-before.json>");
    let after_path = std::env::var("OHARA_PERF_DIFF_AFTER")
        .expect("set OHARA_PERF_DIFF_AFTER=<path-to-after.json>");
    let before = read(Path::new(&before_path));
    let after = read(Path::new(&after_path));

    eprintln!("phase                  before    after    delta");
    eprintln!("---------------------- -------- -------- --------");
    let names: std::collections::BTreeSet<&String> = before
        .phases
        .keys()
        .chain(after.phases.keys())
        .collect();
    for name in names {
        let b = before.phases.get(name).map(|s| s.p50()).unwrap_or(0);
        let a = after.phases.get(name).map(|s| s.p50()).unwrap_or(0);
        let delta = a - b;
        eprintln!("{name:<22} {b:>6}ms {a:>6}ms {delta:>+6}ms");
    }

    if let (Some(b), Some(a)) = (
        before.wall_ms.as_ref().map(|s| s.p50()),
        after.wall_ms.as_ref().map(|s| s.p50()),
    ) {
        eprintln!("wall (cli)             {b:>6}ms {a:>6}ms {delta:>+6}ms", delta = a - b);
    }
    if let (Some(b), Some(a)) = (
        before.find_pattern_wall_ms.as_ref().map(|s| s.p50()),
        after.find_pattern_wall_ms.as_ref().map(|s| s.p50()),
    ) {
        eprintln!("find_pattern (mcp)     {b:>6}ms {a:>6}ms {delta:>+6}ms", delta = a - b);
    }
    if let (Some(b), Some(a)) = (
        before.explain_change_wall_ms.as_ref().map(|s| s.p50()),
        after.explain_change_wall_ms.as_ref().map(|s| s.p50()),
    ) {
        eprintln!("explain_change (mcp)   {b:>6}ms {a:>6}ms {delta:>+6}ms", delta = a - b);
    }
}
```

- [ ] **Step 3: Smoke-test against two runs.**

Run `cli_query_bench` twice (Tasks E.1 step 4 already produced one
run; produce a second, then diff):

```bash
cargo test -p ohara-perf-tests --release -- --ignored cli_query_bench --nocapture
ls -t target/perf/runs/*-cli-query.json | head -2
# pick the two newest reports and pass them in:
OHARA_PERF_DIFF_BEFORE=$(ls -t target/perf/runs/*-cli-query.json | sed -n 2p) \
OHARA_PERF_DIFF_AFTER=$(ls -t target/perf/runs/*-cli-query.json | sed -n 1p) \
cargo test -p ohara-perf-tests --release -- --ignored perf_diff --nocapture
```

Expected: a tabular delta on stderr. Most numbers near 0 since both
runs are against the same code.

- [ ] **Step 4: Commit.**

```bash
git add tests/perf/perf_diff.rs tests/perf/Cargo.toml
git commit -m "test(perf): perf_diff utility for before/after comparisons"
```

---

## Phase F — Documentation

### Task F.1 — Update `tests/perf/README.md`

**Files:**
- Modify: `tests/perf/README.md`

- [ ] **Step 1: Add a "Plan 14 — phase tracing + harness binaries" section.**

Append to `tests/perf/README.md`:

````markdown
## Plan 14 — phase tracing + CLI/MCP harness binaries

Three new operator-run binaries land alongside the existing context-engine
eval and QuestDB baseline:

| File | Purpose |
|---|---|
| `cli_query_bench.rs` | Spawns `ohara query --trace-perf --no-rerank` N times against `fixtures/medium/repo` (ripgrep 14.1.1), parses per-phase stderr, writes a JSON report to `target/perf/runs/<git-sha>-<utc>-cli-query.json`. |
| `mcp_query_bench.rs` | Constructs `OharaServer` in-process, drives `find_pattern` + `explain_change` for N iterations, captures per-phase events via a local subscriber, writes a `*-mcp.json` report. |
| `perf_diff.rs` | Reads two JSON reports via `OHARA_PERF_DIFF_BEFORE`/`OHARA_PERF_DIFF_AFTER` env vars and prints a per-phase tabular delta. |

### Running

```bash
fixtures/build_medium.sh
cargo build --release -p ohara-cli
cargo run --release -p ohara-cli -- index fixtures/medium/repo  # first run only
cargo test -p ohara-perf-tests --release -- --ignored cli_query_bench --nocapture
cargo test -p ohara-perf-tests --release -- --ignored mcp_query_bench --nocapture
```

PR descriptions for any work that claims a CLI/MCP latency win must paste
the `perf_diff` output. Numbers are not CI-gated — operator discipline is.

### `--trace-perf`

The CLI gained a global `--trace-perf` flag that installs an aggregator
on the `ohara::phase` tracing target. End-of-process stderr summary:

```text
[phase] storage_open      8ms   n=1
[phase] embed_load     1820ms   n=1
[phase] embed_query      12ms   n=1
[phase] lane_knn         24ms   n=1   hits=87
[phase] lane_fts_text    18ms   n=1   hits=87
[phase] rerank          780ms   n=1
[phase] hydrate_symbols  45ms   n=1
[phase] total          7042ms
```

Equivalent MCP per-phase data is captured by `mcp_query_bench` directly.
````

- [ ] **Step 2: Commit.**

```bash
git add tests/perf/README.md
git commit -m "docs(perf): document plan-14 phase tracing and harness binaries"
```

---

## Final verification

- [ ] **Step 1: Workspace fmt + clippy + test pass.**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: all clean, all PASS.

- [ ] **Step 2: Confirm no unintended file growth.**

```bash
find crates -name "*.rs" -exec wc -l {} \; | awk '$1 > 500'
```

Expected: no files over 500 lines (per `CONTRIBUTING.md`). If any
appear in this plan's modified set (likely
`crates/ohara-storage/src/storage_impl.rs` since it was already
1497 lines), the file was already over budget — note in the PR but
do not refactor as part of this plan.

- [ ] **Step 3: Run the full perf harness end-to-end.**

```bash
cargo test -p ohara-perf-tests --release -- --ignored cli_query_bench --nocapture
cargo test -p ohara-perf-tests --release -- --ignored mcp_query_bench --nocapture
```

Capture the resulting JSON paths — these are the **before** numbers
that plan-15 PRs will diff against.

---

## Done criteria

- [ ] `timed_phase` helper lives in `ohara-core::perf_trace` with the
  unit test in Task A.1.
- [ ] All retrieval and explain phases emit `ohara::phase` events
  (Tasks A.2, A.3) and CLI / MCP boot phases too (Task A.4).
- [ ] `Storage::metrics_snapshot` returns real numbers from
  `SqliteStorage` and zero from defaults (Task B.2).
- [ ] `RUST_LOG=ohara_storage::sql=trace` produces per-statement
  events (Task B.3).
- [ ] `ohara --trace-perf <subcommand>` prints a per-phase summary on
  stderr at exit (Task C.1).
- [ ] `fixtures/build_medium.sh` is idempotent and pins ripgrep
  14.1.1 via `.fixture-sha` (Task D.1).
- [ ] `cli_query_bench`, `mcp_query_bench`, `perf_diff` produce
  / consume JSON under `target/perf/runs/` (Tasks E.1–E.3).
- [ ] `tests/perf/README.md` documents the new binaries (Task F.1).

After this plan ships, `plan-15` (Phase 2 standalone CLI wins) can
be written with concrete `target/perf/runs/...-cli-query.json`
baseline numbers cited in each task.
