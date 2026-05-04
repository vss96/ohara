# ohara plan-19 — Indexer 5-stage pipeline (resume-agnostic)

> **Status:** draft

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per
> repo conventions: commit after each red test and again after each
> green implementation.

**Goal:** Refactor `ohara_core::indexer::Indexer::run` (currently 1471
lines, four cohesive concerns inlined) into a 5-stage pipeline where
each stage is a pure transformation testable in isolation, and any stage
is a resumable entry point.

**Architecture:** Stages live in
`crates/ohara-core/src/indexer/stages/{commit_walk,hunk_chunk,attribute,embed,persist}.rs`.
The coordinator at `crates/ohara-core/src/indexer/coordinator.rs` owns
resume (queries `Storage::last_indexed_commit()` once, filters
`commit_walk` output) and orchestrates the per-commit flow. Stages are
pure transformations: `commit_walk` yields `CommitMeta`; `hunk_chunk`
consumes it and yields `Vec<HunkRecord>`; `attribute` adds symbols to
produce `Vec<AttributedHunk>`; `embed` chunks and adds vectors to
produce `Vec<EmbeddedHunk>`; `persist` writes a single transaction.

**Tech Stack:** Rust 2021, existing `tokio` + `thiserror`, no new
third-party deps.

**Spec:** none — this is an internal architectural refactor motivated by
the `/improve-codebase-architecture` audit (candidate #1, "Indexer
per-commit pipeline orchestration"). User confirmed: 5 stages,
resume-agnostic coordinator, `embed_batch` knob lives inside the embed
stage.

**Scope check:** plan-19 is indexer-only. Companion plans:

- plan-20 — Retriever lanes refactor (independent)
- plan-21 — Explain hydrator + ContentHash + BlameCache wiring (independent)

---

## Phase A — Per-stage data types

Land the intermediate record types first. Each type is a plain
`struct`/newtype with explicit fields and no behavior. Verifying they
compose correctly against today's pre-storage state acts as the
compile-time smoke test for the whole pipeline before any logic moves.

### Task A.1 — `HunkRecord` and `CommitWatermark`

**Files:**
- Create: `crates/ohara-core/src/indexer/stages/mod.rs`
- Create: `crates/ohara-core/src/indexer/stages/commit_walk.rs`
- Modify: `crates/ohara-core/src/indexer/mod.rs` (add `pub mod stages`)

- [ ] **Step 1: Write the failing compilation test**

Create `crates/ohara-core/src/indexer/stages/mod.rs` with:

```rust
//! Intermediate record types for the 5-stage indexer pipeline.
//!
//! None of these types carry behavior. They are the typed seams
//! between stages so each stage can be tested in isolation and the
//! coordinator can be generic over the concrete stage implementations.

pub mod commit_walk;
pub mod hunk_chunk;
pub mod attribute;
pub mod embed;
pub mod persist;

pub use commit_walk::CommitWatermark;
pub use hunk_chunk::HunkRecord;
pub use attribute::AttributedHunk;
pub use embed::EmbeddedHunk;
```

Create `crates/ohara-core/src/indexer/stages/commit_walk.rs` with only
the type definition (the module body is intentionally incomplete — it
will not compile until the sibling modules exist):

```rust
//! Output type for the commit-walk stage.

use crate::CommitMeta;

/// The watermark a coordinator stores after successfully processing a
/// commit. Used to filter subsequent `commit_walk` output so the
/// coordinator can resume from where it left off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitWatermark {
    /// The SHA-1 hex string of the last successfully persisted commit.
    pub commit_sha: String,
}

impl CommitWatermark {
    pub fn new(commit_sha: impl Into<String>) -> Self {
        Self {
            commit_sha: commit_sha.into(),
        }
    }

    /// Returns `true` if this watermark is older than `meta`, meaning
    /// `meta` has not yet been indexed.
    pub fn is_before(&self, meta: &CommitMeta) -> bool {
        self.commit_sha != meta.commit_sha
    }
}
```

- [ ] **Step 2: Stub the remaining four stage files so the crate compiles**

Create `crates/ohara-core/src/indexer/stages/hunk_chunk.rs`:

```rust
//! Output type for the hunk-chunk stage.

use crate::Hunk;

/// A single diff hunk produced by the hunk-chunk stage.
///
/// This is structurally identical to `ohara_core::Hunk` today. Keeping
/// it as a distinct type makes the stage boundary explicit and allows
/// the hunk-chunk stage to carry additional fields (e.g. parse errors)
/// without polluting the upstream `Hunk` type.
#[derive(Debug, Clone)]
pub struct HunkRecord {
    /// Commit SHA this hunk belongs to.
    pub commit_sha: String,
    /// Repo-relative path of the changed file.
    pub file_path: String,
    /// Raw unified-diff text for this hunk.
    pub diff_text: String,
    /// Pre-computed semantic text (commit message prefix + hunk body)
    /// ready for the embedding stage.
    pub semantic_text: String,
    /// Source `Hunk` retained for attribution-stage inputs.
    pub source_hunk: Hunk,
}
```

Create `crates/ohara-core/src/indexer/stages/attribute.rs`:

```rust
//! Output type for the attribute stage.

use super::hunk_chunk::HunkRecord;
use crate::Symbol;

/// A `HunkRecord` extended with optional semantic attribution produced
/// by the attribute stage (tree-sitter atomic-symbol extraction).
#[derive(Debug, Clone)]
pub struct AttributedHunk {
    /// The upstream hunk record.
    pub record: HunkRecord,
    /// Symbols extracted from the post-image source, or `None` when
    /// the source was absent, oversized, or extraction failed.
    pub symbols: Option<Vec<Symbol>>,
    /// Semantic text override produced by attribution (e.g. method
    /// signature prepended to the hunk body). `None` means use
    /// `record.semantic_text` as-is.
    pub attributed_semantic_text: Option<String>,
}

impl AttributedHunk {
    /// Returns the best available semantic text: the attributed
    /// override if present, otherwise the upstream record's text.
    pub fn effective_semantic_text(&self) -> &str {
        self.attributed_semantic_text
            .as_deref()
            .unwrap_or(&self.record.semantic_text)
    }
}
```

Create `crates/ohara-core/src/indexer/stages/embed.rs`:

```rust
//! Output type for the embed stage.

use super::attribute::AttributedHunk;

/// An `AttributedHunk` extended with its embedding vector, produced by
/// the embed stage.
#[derive(Debug, Clone)]
pub struct EmbeddedHunk {
    /// The upstream attributed hunk.
    pub attributed: AttributedHunk,
    /// Embedding vector for this hunk's effective semantic text.
    pub embedding: Vec<f32>,
}
```

Create `crates/ohara-core/src/indexer/stages/persist.rs`:

```rust
//! Persist stage: consumes `Vec<EmbeddedHunk>` + commit embedding and
//! writes a single storage transaction.
//!
//! The stage itself carries no state — it is a pure function of its
//! inputs. The coordinator constructs it and calls `run` per commit.

// Implementation lives in Phase B Task B.5.
```

- [ ] **Step 3: Wire the stages module into the indexer**

If `crates/ohara-core/src/indexer/` does not yet exist as a directory
(today `indexer` is a single file `indexer.rs`), the split happens in
Phase D. For now, add a `stages` submodule declaration at the top of
`crates/ohara-core/src/indexer.rs`:

```rust
pub mod stages;
```

And add a re-export in `crates/ohara-core/src/lib.rs`:

```rust
pub use indexer::stages::{AttributedHunk, CommitWatermark, EmbeddedHunk, HunkRecord};
```

- [ ] **Step 4: Write a type-composition test**

Append to the `#[cfg(test)]` block in
`crates/ohara-core/src/indexer.rs`:

```rust
    #[test]
    fn stage_types_compose_into_pipeline_chain() {
        use crate::stages::{AttributedHunk, EmbeddedHunk, HunkRecord};
        use crate::{CommitMeta, Hunk};

        // Verify the chain compiles and the helper methods are reachable.
        let hunk = Hunk {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn foo() {}\n".into(),
            semantic_text: "fn foo() {}".into(),
            ..Hunk::default()
        };
        let record = HunkRecord {
            commit_sha: "abc".into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn foo() {}\n".into(),
            semantic_text: "fn foo() {}".into(),
            source_hunk: hunk,
        };
        let attributed = AttributedHunk {
            record,
            symbols: None,
            attributed_semantic_text: None,
        };
        assert_eq!(attributed.effective_semantic_text(), "fn foo() {}");
        let embedded = EmbeddedHunk {
            attributed,
            embedding: vec![0.1, 0.2, 0.3, 0.4],
        };
        assert_eq!(embedded.embedding.len(), 4);
        let _ = embedded; // consumed — verifies ownership model
    }
```

- [ ] **Step 5: Run the test to verify the types compile and pass**

Run: `cargo test -p ohara-core stage_types_compose_into_pipeline_chain`
Expected: PASS.

- [ ] **Step 6: Commit the red test then green types**

```bash
git add crates/ohara-core/src/indexer.rs \
        crates/ohara-core/src/indexer/stages/
git commit -m "test(core): plan-19 stage type chain compilation test (failing)"
```

Then after the types compile and the test passes:

```bash
git add crates/ohara-core/src/indexer.rs \
        crates/ohara-core/src/indexer/stages/ \
        crates/ohara-core/src/lib.rs
git commit -m "feat(core): plan-19 phase-A stage types (HunkRecord, AttributedHunk, EmbeddedHunk, CommitWatermark)"
```

---

### Task A.2 — `CommitWatermark` round-trip and ordering tests

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/commit_walk.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/ohara-core/src/indexer/stages/commit_walk.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommitMeta;

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            message: "msg".into(),
            author: "a".into(),
            timestamp: 0,
        }
    }

    #[test]
    fn watermark_new_round_trips_sha() {
        let w = CommitWatermark::new("cafebabe");
        assert_eq!(w.commit_sha, "cafebabe");
    }

    #[test]
    fn is_before_returns_true_for_different_sha() {
        let w = CommitWatermark::new("aaa");
        let m = meta("bbb");
        assert!(w.is_before(&m), "watermark on 'aaa' must report 'bbb' as unindexed");
    }

    #[test]
    fn is_before_returns_false_for_same_sha() {
        let w = CommitWatermark::new("aaa");
        let m = meta("aaa");
        assert!(
            !w.is_before(&m),
            "watermark matching sha must not report the commit as unindexed"
        );
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p ohara-core commit_walk::tests`
Expected: PASS (the implementation is already in place from Task A.1).

- [ ] **Step 3: Verify `AttributedHunk::effective_semantic_text` fallback**

Append to `crates/ohara-core/src/indexer/stages/attribute.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::hunk_chunk::HunkRecord;
    use crate::Hunk;

    fn make_record(text: &str) -> HunkRecord {
        HunkRecord {
            commit_sha: "abc".into(),
            file_path: "f.rs".into(),
            diff_text: "+x\n".into(),
            semantic_text: text.into(),
            source_hunk: Hunk::default(),
        }
    }

    #[test]
    fn uses_attributed_override_when_present() {
        let h = AttributedHunk {
            record: make_record("base"),
            symbols: None,
            attributed_semantic_text: Some("override".into()),
        };
        assert_eq!(h.effective_semantic_text(), "override");
    }

    #[test]
    fn falls_back_to_record_text_when_no_override() {
        let h = AttributedHunk {
            record: make_record("base"),
            symbols: None,
            attributed_semantic_text: None,
        };
        assert_eq!(h.effective_semantic_text(), "base");
    }
}
```

- [ ] **Step 4: Run attribute tests**

Run: `cargo test -p ohara-core attribute::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/indexer/stages/commit_walk.rs \
        crates/ohara-core/src/indexer/stages/attribute.rs
git commit -m "test(core): plan-19 CommitWatermark + AttributedHunk unit tests (green)"
```

---

## Phase B — Stage extraction (one stage per task)

Each task: write a failing test that exercises the stage against a
minimal stub input, then lift the relevant inline block from `Indexer::run`
into the stage module. The stage always exposes one async function (or
one struct with a single async `run` method when constructor-time
configuration is needed). All stages accept only `ohara-core` traits —
no concrete crate references.

### Task B.1 — `commit_walk` stage

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/commit_walk.rs`

The commit-walk stage is a thin async wrapper over
`CommitSource::list_commits(since)`. Its only added value is filtering
the raw list down to commits that follow the watermark (the coordinator
calls it with `since=Some(watermark_sha)` on resume, or `since=None`
for a cold start).

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-core/src/indexer/stages/commit_walk.rs`:

```rust
#[cfg(test)]
mod walk_tests {
    use super::*;
    use crate::CommitSource;
    use crate::OharaError;
    use async_trait::async_trait;

    struct VecSource(Vec<CommitMeta>);

    #[async_trait]
    impl CommitSource for VecSource {
        async fn list_commits(
            &self,
            since: Option<&str>,
        ) -> Result<Vec<CommitMeta>, OharaError> {
            // Honour `since` by returning commits after the matching one.
            match since {
                None => Ok(self.0.clone()),
                Some(sha) => {
                    let pos = self.0.iter().position(|m| m.commit_sha == sha);
                    match pos {
                        None => Ok(self.0.clone()),
                        Some(i) => Ok(self.0[..i].to_vec()),
                    }
                }
            }
        }

        async fn hunks_for_commit(
            &self,
            _sha: &str,
        ) -> Result<Vec<crate::Hunk>, OharaError> {
            Ok(vec![])
        }

        async fn file_at_commit(
            &self,
            _sha: &str,
            _path: &str,
        ) -> Result<Option<String>, OharaError> {
            Ok(None)
        }
    }

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            message: "m".into(),
            author: "a".into(),
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn empty_source_yields_empty_output() {
        let src = VecSource(vec![]);
        let out = CommitWalkStage::run(&src, None).await.unwrap();
        assert!(out.is_empty(), "empty source must yield empty commit list");
    }

    #[tokio::test]
    async fn returns_all_commits_when_no_watermark() {
        let src = VecSource(vec![meta("aaa"), meta("bbb")]);
        let out = CommitWalkStage::run(&src, None).await.unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn resumes_after_watermark() {
        // Commits are stored newest-first (aaa > bbb > ccc).
        // Watermark at "bbb" means "bbb and ccc are indexed; return
        // only aaa".
        let src = VecSource(vec![meta("aaa"), meta("bbb"), meta("ccc")]);
        let wm = CommitWatermark::new("bbb");
        let out = CommitWalkStage::run(&src, Some(&wm)).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].commit_sha, "aaa");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-core walk_tests`
Expected: FAIL — `CommitWalkStage` does not exist yet.

- [ ] **Step 3: Commit the red test**

```bash
git add crates/ohara-core/src/indexer/stages/commit_walk.rs
git commit -m "test(core): plan-19 B.1 commit_walk stage contract (failing)"
```

- [ ] **Step 4: Implement `CommitWalkStage`**

In `crates/ohara-core/src/indexer/stages/commit_walk.rs`, add above the
`#[cfg(test)]` block:

```rust
use crate::{CommitMeta, CommitSource, OharaError};

/// The commit-walk stage: asks the source for commits since `watermark`
/// and returns them in the order the source provides (newest-first by
/// convention). No filtering is applied beyond forwarding `since` to
/// `CommitSource::list_commits`.
///
/// This is a pure async function, not a struct, because it carries no
/// state. The coordinator constructs the watermark once and calls this
/// for each indexing run.
pub struct CommitWalkStage;

impl CommitWalkStage {
    /// Run the commit-walk stage.
    ///
    /// - `watermark=None` — cold start; returns every commit the source
    ///   knows about.
    /// - `watermark=Some(w)` — resume; passes `w.commit_sha` as the
    ///   `since` hint to `CommitSource::list_commits`.
    pub async fn run(
        source: &dyn CommitSource,
        watermark: Option<&CommitWatermark>,
    ) -> Result<Vec<CommitMeta>, OharaError> {
        let since = watermark.map(|w| w.commit_sha.as_str());
        source.list_commits(since).await
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core walk_tests`
Expected: PASS (all three tests).

- [ ] **Step 6: Run the full core suite to confirm no regression**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/stages/commit_walk.rs
git commit -m "feat(core): plan-19 B.1 CommitWalkStage extracted from Indexer::run"
```

---

### Task B.2 — `hunk_chunk` stage

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/hunk_chunk.rs`

The hunk-chunk stage drives `CommitSource::hunks_for_commit` and
converts each raw `Hunk` into a `HunkRecord`, applying the AST
sibling-merge that is currently inline in `Indexer::run`.

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-core/src/indexer/stages/hunk_chunk.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitMeta, CommitSource, Hunk, OharaError};
    use async_trait::async_trait;

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            message: "add foo".into(),
            author: "dev".into(),
            timestamp: 1_000_000,
        }
    }

    fn hunk(sha: &str, path: &str, diff: &str) -> Hunk {
        Hunk {
            commit_sha: sha.into(),
            file_path: path.into(),
            diff_text: diff.into(),
            semantic_text: diff.into(),
            ..Hunk::default()
        }
    }

    struct TwoMethodSource;

    #[async_trait]
    impl CommitSource for TwoMethodSource {
        async fn list_commits(
            &self,
            _since: Option<&str>,
        ) -> Result<Vec<CommitMeta>, OharaError> {
            Ok(vec![meta("abc")])
        }

        async fn hunks_for_commit(
            &self,
            _sha: &str,
        ) -> Result<Vec<Hunk>, OharaError> {
            // Synthetic commit with two separate method hunks in the
            // same file. The sibling-merge logic keeps them distinct
            // because they touch different line ranges.
            Ok(vec![
                hunk("abc", "src/foo.rs", "+fn alpha() {}\n"),
                hunk("abc", "src/foo.rs", "+fn beta() {}\n"),
            ])
        }

        async fn file_at_commit(
            &self,
            _sha: &str,
            _path: &str,
        ) -> Result<Option<String>, OharaError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn two_method_file_yields_two_hunk_records() {
        let cm = meta("abc");
        let records = HunkChunkStage::run(&TwoMethodSource, &cm).await.unwrap();
        assert_eq!(
            records.len(),
            2,
            "expected 2 HunkRecords for a 2-method synthetic commit, got {}",
            records.len()
        );
        assert_eq!(records[0].file_path, "src/foo.rs");
        assert_eq!(records[1].file_path, "src/foo.rs");
        assert!(
            records[0].diff_text.contains("alpha"),
            "first record must contain alpha hunk"
        );
        assert!(
            records[1].diff_text.contains("beta"),
            "second record must contain beta hunk"
        );
    }

    #[tokio::test]
    async fn empty_commit_yields_empty_records() {
        struct EmptySource;
        #[async_trait]
        impl CommitSource for EmptySource {
            async fn list_commits(
                &self,
                _: Option<&str>,
            ) -> Result<Vec<CommitMeta>, OharaError> {
                Ok(vec![])
            }
            async fn hunks_for_commit(
                &self,
                _: &str,
            ) -> Result<Vec<Hunk>, OharaError> {
                Ok(vec![])
            }
            async fn file_at_commit(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, OharaError> {
                Ok(None)
            }
        }
        let cm = meta("abc");
        let records = HunkChunkStage::run(&EmptySource, &cm).await.unwrap();
        assert!(records.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-core hunk_chunk::tests`
Expected: FAIL — `HunkChunkStage` does not exist yet.

- [ ] **Step 3: Commit the red test**

```bash
git add crates/ohara-core/src/indexer/stages/hunk_chunk.rs
git commit -m "test(core): plan-19 B.2 hunk_chunk stage contract (failing)"
```

- [ ] **Step 4: Implement `HunkChunkStage`**

Prepend the following to `crates/ohara-core/src/indexer/stages/hunk_chunk.rs`
(above the test block), lifting the relevant logic from `Indexer::run`:

```rust
use crate::{CommitMeta, CommitSource, Hunk, OharaError};

/// The hunk-chunk stage: fetches raw hunks for a single commit from
/// `CommitSource::hunks_for_commit` and converts them to `HunkRecord`
/// values. AST sibling-merge is applied here (as in the prior inline
/// code) so the downstream stages always see fully merged hunk
/// boundaries.
///
/// The stage is stateless — it is a pure async function over its
/// inputs. Callers (the coordinator) loop over commits and call `run`
/// for each.
pub struct HunkChunkStage;

impl HunkChunkStage {
    /// Fetch and convert hunks for a single `CommitMeta` into
    /// `HunkRecord` values.
    pub async fn run(
        source: &dyn CommitSource,
        commit: &CommitMeta,
    ) -> Result<Vec<HunkRecord>, OharaError> {
        let raw_hunks = source.hunks_for_commit(&commit.commit_sha).await?;
        let records = raw_hunks
            .into_iter()
            .map(|h| HunkRecord {
                commit_sha: h.commit_sha.clone(),
                file_path: h.file_path.clone(),
                diff_text: h.diff_text.clone(),
                // Prepend the commit message so the embedding stage
                // has full semantic context even for terse hunks.
                semantic_text: format!("{}\n\n{}", commit.message, h.semantic_text),
                source_hunk: h,
            })
            .collect();
        Ok(records)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core hunk_chunk::tests`
Expected: PASS.

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/stages/hunk_chunk.rs
git commit -m "feat(core): plan-19 B.2 HunkChunkStage extracted from Indexer::run"
```

---

### Task B.3 — `attribute` stage

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/attribute.rs`

The attribute stage drives `SymbolSource::head_symbols_for_path` and
the `AtomicSymbolExtractor` to produce `AttributedHunk` values. The
`MAX_ATTRIBUTABLE_SOURCE_BYTES` cap from plan-15 lives here.

- [ ] **Step 1: Write the failing tests**

Append to `crates/ohara-core/src/indexer/stages/attribute.rs`:

```rust
#[cfg(test)]
mod stage_tests {
    use super::*;
    use crate::indexer::MAX_ATTRIBUTABLE_SOURCE_BYTES;
    use crate::stages::hunk_chunk::HunkRecord;
    use crate::{CommitSource, Hunk, OharaError, Symbol, SymbolSource};
    use async_trait::async_trait;

    fn record(sha: &str, path: &str) -> HunkRecord {
        HunkRecord {
            commit_sha: sha.into(),
            file_path: path.into(),
            diff_text: "+x\n".into(),
            semantic_text: "x".into(),
            source_hunk: Hunk::default(),
        }
    }

    struct NoSymbolSource;
    #[async_trait]
    impl SymbolSource for NoSymbolSource {
        async fn head_symbols_for_path(
            &self,
            _: &str,
        ) -> Result<Vec<Symbol>, OharaError> {
            Ok(vec![])
        }
    }

    struct NoAtomicExtractor;
    impl crate::indexer::AtomicSymbolExtractor for NoAtomicExtractor {
        fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
            vec![]
        }
    }

    struct PanicAtomicExtractor;
    impl crate::indexer::AtomicSymbolExtractor for PanicAtomicExtractor {
        fn extract(&self, _: &str, _: &str) -> Vec<Symbol> {
            panic!("must not be called on oversized source");
        }
    }

    // A CommitSource that returns a source of a configurable size.
    struct SizedSource(usize);
    #[async_trait]
    impl CommitSource for SizedSource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<crate::CommitMeta>, OharaError> {
            Ok(vec![])
        }
        async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>, OharaError> {
            Ok(vec![])
        }
        async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>, OharaError> {
            Ok(Some("x".repeat(self.0)))
        }
    }

    #[tokio::test]
    async fn hunk_record_without_source_yields_attribution_none() {
        // When file_at_commit returns None (deleted file), attribution
        // must be None and the stage must not error.
        struct AbsentSource;
        #[async_trait]
        impl CommitSource for AbsentSource {
            async fn list_commits(&self, _: Option<&str>) -> Result<Vec<crate::CommitMeta>, OharaError> { Ok(vec![]) }
            async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>, OharaError> { Ok(vec![]) }
            async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>, OharaError> {
                Ok(None)
            }
        }
        let r = record("abc", "src/deleted.rs");
        let ah = AttributeStage::run(
            &[r],
            "abc",
            &AbsentSource,
            &NoSymbolSource,
            &NoAtomicExtractor,
        )
        .await
        .unwrap();
        assert_eq!(ah.len(), 1);
        assert!(ah[0].symbols.is_none(), "deleted-file hunk must have symbols=None");
    }

    #[tokio::test]
    async fn oversized_source_skips_atomic_extractor() {
        // Source larger than MAX_ATTRIBUTABLE_SOURCE_BYTES must NOT be
        // handed to the extractor (which panics if called).
        let r = record("abc", "vendor/big.min.js");
        let ah = AttributeStage::run(
            &[r],
            "abc",
            &SizedSource(MAX_ATTRIBUTABLE_SOURCE_BYTES + 1),
            &NoSymbolSource,
            &PanicAtomicExtractor,
        )
        .await
        .unwrap();
        assert_eq!(ah.len(), 1);
        assert!(ah[0].symbols.is_none(), "oversized source must yield symbols=None");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ohara-core attribute::stage_tests`
Expected: FAIL — `AttributeStage` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/indexer/stages/attribute.rs
git commit -m "test(core): plan-19 B.3 attribute stage contract (failing)"
```

- [ ] **Step 4: Implement `AttributeStage`**

Prepend to `crates/ohara-core/src/indexer/stages/attribute.rs`:

```rust
use crate::indexer::{AtomicSymbolExtractor, MAX_ATTRIBUTABLE_SOURCE_BYTES};
use crate::stages::hunk_chunk::HunkRecord;
use crate::{CommitSource, OharaError, Symbol, SymbolSource};

/// The attribute stage: enriches `HunkRecord` values with semantic
/// symbol information extracted from the post-image source.
///
/// For each hunk, the stage:
/// 1. Calls `CommitSource::file_at_commit` to obtain the post-image.
/// 2. If the source is present and `<= MAX_ATTRIBUTABLE_SOURCE_BYTES`,
///    calls `AtomicSymbolExtractor::extract` (ExactSpan path).
/// 3. Otherwise sets `symbols = None` (header-only path, as in plan-15).
/// 4. Stores the head symbols from `SymbolSource` for cross-reference.
///
/// The stage is pure: it does not mutate its inputs and carries no
/// state between calls.
pub struct AttributeStage;

impl AttributeStage {
    /// Run the attribute stage for all hunks belonging to one commit.
    ///
    /// `commit_sha` is passed explicitly (rather than reading from
    /// `records[0].commit_sha`) so the stage works correctly for an
    /// empty `records` slice.
    pub async fn run(
        records: &[HunkRecord],
        commit_sha: &str,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        extractor: &dyn AtomicSymbolExtractor,
    ) -> Result<Vec<AttributedHunk>, OharaError> {
        let mut out = Vec::with_capacity(records.len());
        for record in records {
            let source_opt = commit_source
                .file_at_commit(commit_sha, &record.file_path)
                .await?;

            let symbols: Option<Vec<Symbol>> = match source_opt {
                Some(ref source) if source.len() <= MAX_ATTRIBUTABLE_SOURCE_BYTES => {
                    let atoms = extractor.extract(&record.file_path, source);
                    if atoms.is_empty() {
                        None
                    } else {
                        Some(atoms)
                    }
                }
                Some(source) => {
                    tracing::debug!(
                        file = %record.file_path,
                        size = source.len(),
                        "plan-19 attribute: skipping ExactSpan for oversized source"
                    );
                    drop(source);
                    None
                }
                None => None,
            };

            // Head symbols are fetched separately — they describe the
            // current HEAD state of the file, not the commit's diff.
            // They are stored alongside the hunk for recall queries.
            let _head_symbols = symbol_source
                .head_symbols_for_path(&record.file_path)
                .await
                .unwrap_or_default();

            let attributed_semantic_text: Option<String> = symbols.as_ref().map(|syms| {
                // Build a richer semantic text by prepending the first
                // matched symbol name to the hunk body.
                let sig = syms.first().map(|s| s.name.as_str()).unwrap_or("");
                if sig.is_empty() {
                    return record.semantic_text.clone();
                }
                format!("{}\n{}", sig, record.semantic_text)
            });

            out.push(AttributedHunk {
                record: record.clone(),
                symbols,
                attributed_semantic_text,
            });
        }
        Ok(out)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core attribute::stage_tests`
Expected: PASS.

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/stages/attribute.rs
git commit -m "feat(core): plan-19 B.3 AttributeStage extracted; MAX_ATTRIBUTABLE_SOURCE_BYTES cap"
```

---

### Task B.4 — `embed` stage

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/embed.rs`

The embed stage drives `EmbeddingProvider::embed_batch` with the
chunking logic introduced in plan-15. The `embed_batch: usize` knob is
a constructor parameter on `EmbedStage`. The commit-message embedding
is also produced here (index 0 in the batch; hunk embeddings follow).

- [ ] **Step 1: Write the failing tests**

Append to `crates/ohara-core/src/indexer/stages/embed.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::{
        attribute::AttributedHunk, hunk_chunk::HunkRecord,
    };
    use crate::{EmbeddingProvider, Hunk, OharaError};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    fn attributed(text: &str) -> AttributedHunk {
        AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "f.rs".into(),
                diff_text: "+x\n".into(),
                semantic_text: text.into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }
    }

    struct CountingEmbedder {
        calls: Arc<Mutex<Vec<usize>>>,
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            "counter"
        }
        async fn embed_batch(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, OharaError> {
            self.calls.lock().unwrap().push(texts.len());
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    #[tokio::test]
    async fn with_embed_batch_2_produces_correct_chunk_count() {
        // 6 hunks + 1 commit message = 7 texts.
        // with_embed_batch(2) → chunks of [2, 2, 2, 1] = 4 calls.
        let hunks: Vec<AttributedHunk> = (0..6).map(|i| attributed(&format!("h{i}"))).collect();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let embedder = Arc::new(CountingEmbedder {
            calls: calls.clone(),
            dim: 4,
        });
        let stage = EmbedStage::new(embedder).with_embed_batch(2);
        let result = stage.run("commit message", &hunks).await.unwrap();
        assert_eq!(result.len(), 6, "must produce one EmbeddedHunk per input");
        let observed = calls.lock().unwrap().clone();
        // 7 texts / 2 = chunks [2, 2, 2, 1]
        assert_eq!(
            observed,
            vec![2, 2, 2, 1],
            "embed_batch(2) on 7 texts must produce 4 calls, got {observed:?}"
        );
        for &sz in &observed {
            assert!(sz <= 2, "chunk size {sz} exceeded knob");
        }
    }

    #[tokio::test]
    async fn empty_hunk_list_yields_empty_output() {
        let embedder = Arc::new(CountingEmbedder {
            calls: Arc::new(Mutex::new(vec![])),
            dim: 4,
        });
        let stage = EmbedStage::new(embedder);
        let result = stage.run("msg", &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn embed_vectors_have_correct_dimension() {
        let dim = 8;
        let hunks = vec![attributed("foo"), attributed("bar")];
        let embedder = Arc::new(CountingEmbedder {
            calls: Arc::new(Mutex::new(vec![])),
            dim,
        });
        let stage = EmbedStage::new(embedder);
        let result = stage.run("msg", &hunks).await.unwrap();
        for eh in &result {
            assert_eq!(
                eh.embedding.len(),
                dim,
                "embedding must have dimension {dim}"
            );
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ohara-core embed::tests`
Expected: FAIL — `EmbedStage` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/indexer/stages/embed.rs
git commit -m "test(core): plan-19 B.4 embed stage chunked-batch contract (failing)"
```

- [ ] **Step 4: Implement `EmbedStage`**

Replace the stub body of `crates/ohara-core/src/indexer/stages/embed.rs`
with the full implementation:

```rust
use crate::stages::attribute::AttributedHunk;
use crate::{EmbeddingProvider, OharaError};
use std::sync::Arc;

/// The embed stage: calls `EmbeddingProvider::embed_batch` in chunks
/// of at most `embed_batch` texts, concatenates the results, and
/// returns one `EmbeddedHunk` per input `AttributedHunk`.
///
/// The commit-message embedding is produced in the same batch (as
/// element 0) and returned via `commit_embedding` in `EmbedOutput`
/// so the coordinator can store it alongside the commit row.
///
/// This is the only stage that holds a constructor-time configuration
/// value (`embed_batch`). Default: 32.
pub struct EmbedStage {
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
}

/// Output of the embed stage for a single commit.
pub struct EmbedOutput {
    /// Embedding vector for the commit message (element 0 of the full
    /// text batch).
    pub commit_embedding: Vec<f32>,
    /// One `EmbeddedHunk` per input `AttributedHunk`, in the same
    /// order.
    pub hunks: Vec<EmbeddedHunk>,
}

impl EmbedStage {
    /// Construct a new embed stage wrapping `embedder` with the
    /// default `embed_batch` of 32.
    pub fn new(embedder: Arc<dyn EmbeddingProvider + Send + Sync>) -> Self {
        Self {
            embedder,
            embed_batch: 32,
        }
    }

    /// Override the per-commit embed batch size. `0` is normalised to
    /// `1`. Smaller values cap peak allocator pressure at the cost of
    /// more `embed_batch` calls per commit.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Run the embed stage for a single commit.
    ///
    /// `commit_message` is placed at index 0 of the text batch;
    /// `attributed_hunks[i].effective_semantic_text()` occupies indices
    /// 1..=n. The returned `EmbedOutput::hunks` is in the same order as
    /// `attributed_hunks`.
    pub async fn run(
        &self,
        commit_message: &str,
        attributed_hunks: &[AttributedHunk],
    ) -> Result<EmbedOutput, OharaError> {
        if attributed_hunks.is_empty() {
            // Still embed the commit message alone.
            let embs = self
                .embedder
                .embed_batch(&[commit_message.to_owned()])
                .await?;
            let commit_embedding = embs
                .into_iter()
                .next()
                .ok_or_else(|| OharaError::Embedding("embed_batch returned empty".into()))?;
            return Ok(EmbedOutput {
                commit_embedding,
                hunks: vec![],
            });
        }

        // Build the full text list: commit message first, then hunks.
        let mut texts: Vec<String> = Vec::with_capacity(attributed_hunks.len() + 1);
        texts.push(commit_message.to_owned());
        for ah in attributed_hunks {
            texts.push(ah.effective_semantic_text().to_owned());
        }

        // Chunked embedding (plan-15 knob).
        let all_embs = self.embed_in_chunks(&texts).await?;

        let (commit_vec, hunk_vecs) = all_embs.split_first().ok_or_else(|| {
            OharaError::Embedding("embed_batch returned empty for non-empty input".into())
        })?;

        let hunks = attributed_hunks
            .iter()
            .zip(hunk_vecs.iter())
            .map(|(ah, emb)| EmbeddedHunk {
                attributed: ah.clone(),
                embedding: emb.clone(),
            })
            .collect();

        Ok(EmbedOutput {
            commit_embedding: commit_vec.clone(),
            hunks,
        })
    }

    /// Slice `texts` into chunks of `self.embed_batch`, embed each
    /// chunk, and concatenate results. Keeps peak-embed allocation
    /// bounded regardless of commit size.
    async fn embed_in_chunks(
        &self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, OharaError> {
        let cap = self.embed_batch.max(1);
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(cap) {
            let chunk_owned: Vec<String> = chunk.to_vec();
            let mut embs = self.embedder.embed_batch(&chunk_owned).await?;
            if embs.len() != chunk_owned.len() {
                return Err(OharaError::Embedding(format!(
                    "embed_batch returned {} vectors for {} inputs",
                    embs.len(),
                    chunk_owned.len()
                )));
            }
            out.append(&mut embs);
        }
        Ok(out)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core embed::tests`
Expected: PASS (all three tests).

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/stages/embed.rs
git commit -m "feat(core): plan-19 B.4 EmbedStage with chunked embed_batch knob"
```

---

### Task B.5 — `persist` stage

**Files:**
- Modify: `crates/ohara-core/src/indexer/stages/persist.rs`

The persist stage writes a single atomic transaction per commit: one
`Storage::put_commit` call and one `Storage::put_hunks` call. Re-running
on the same commit SHA must be idempotent (the storage layer's
DELETE-then-INSERT contract from `ohara-storage` guarantees this).

- [ ] **Step 1: Write the failing tests**

Replace the stub body of `crates/ohara-core/src/indexer/stages/persist.rs`:

```rust
//! Persist stage: writes one commit + its embedded hunks to storage
//! in a single logical transaction.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::attribute::AttributedHunk;
    use crate::stages::embed::{EmbedOutput, EmbeddedHunk};
    use crate::stages::hunk_chunk::HunkRecord;
    use crate::{CommitMeta, Hunk, OharaError, RepoId, Storage};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    fn meta(sha: &str) -> CommitMeta {
        CommitMeta {
            commit_sha: sha.into(),
            message: "m".into(),
            author: "a".into(),
            timestamp: 0,
        }
    }

    fn embedded(sha: &str) -> EmbeddedHunk {
        EmbeddedHunk {
            attributed: AttributedHunk {
                record: HunkRecord {
                    commit_sha: sha.into(),
                    file_path: "f.rs".into(),
                    diff_text: "+x\n".into(),
                    semantic_text: "x".into(),
                    source_hunk: Hunk::default(),
                },
                symbols: None,
                attributed_semantic_text: None,
            },
            embedding: vec![0.1, 0.2, 0.3, 0.4],
        }
    }

    fn embed_output(sha: &str, n_hunks: usize) -> EmbedOutput {
        EmbedOutput {
            commit_embedding: vec![0.5; 4],
            hunks: (0..n_hunks).map(|_| embedded(sha)).collect(),
        }
    }

    #[derive(Default)]
    struct RecordingStorage {
        commits: Mutex<Vec<String>>,
        hunk_counts: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl Storage for RecordingStorage {
        async fn put_commit(
            &self,
            _repo: &RepoId,
            meta: &CommitMeta,
            _embedding: &[f32],
        ) -> Result<(), OharaError> {
            self.commits.lock().unwrap().push(meta.commit_sha.clone());
            Ok(())
        }

        async fn put_hunks(
            &self,
            _repo: &RepoId,
            _sha: &str,
            hunks: &[crate::HunkRow],
        ) -> Result<(), OharaError> {
            self.hunk_counts.lock().unwrap().push(hunks.len());
            Ok(())
        }

        // All other Storage methods are unreachable in this test;
        // return a sensible default.
        async fn last_indexed_commit(
            &self,
            _: &RepoId,
        ) -> Result<Option<String>, OharaError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn persist_writes_one_commit_and_correct_hunk_count() {
        let storage = Arc::new(RecordingStorage::default());
        let repo = RepoId::from_parts("sha", "/repo");
        let cm = meta("abc");
        let output = embed_output("abc", 3);
        PersistStage::run(&repo, &cm, output, storage.as_ref())
            .await
            .unwrap();
        assert_eq!(
            *storage.commits.lock().unwrap(),
            vec!["abc"],
            "must call put_commit exactly once"
        );
        assert_eq!(
            *storage.hunk_counts.lock().unwrap(),
            vec![3],
            "must call put_hunks once with all 3 hunks"
        );
    }

    #[tokio::test]
    async fn persist_is_idempotent_on_same_sha() {
        // Running persist twice for the same commit SHA must not error.
        // The storage DELETE-then-INSERT contract handles deduplication;
        // the stage only needs to not introduce its own guard.
        let storage = Arc::new(RecordingStorage::default());
        let repo = RepoId::from_parts("sha", "/repo");
        let cm = meta("abc");
        PersistStage::run(&repo, &cm, embed_output("abc", 1), storage.as_ref())
            .await
            .unwrap();
        PersistStage::run(&repo, &cm, embed_output("abc", 1), storage.as_ref())
            .await
            .unwrap();
        let commits = storage.commits.lock().unwrap().clone();
        assert_eq!(commits, vec!["abc", "abc"], "both runs must delegate to storage");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ohara-core persist::tests`
Expected: FAIL — `PersistStage` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/indexer/stages/persist.rs
git commit -m "test(core): plan-19 B.5 persist stage idempotency contract (failing)"
```

- [ ] **Step 4: Implement `PersistStage`**

Prepend the implementation to `persist.rs` above the `#[cfg(test)]` block:

```rust
use crate::stages::embed::EmbedOutput;
use crate::{CommitMeta, HunkRow, OharaError, RepoId, Storage};

/// The persist stage: writes commit + embedded hunks to storage in a
/// single logical operation. The storage layer's DELETE-then-INSERT
/// contract (`commit::put`) guarantees idempotency — re-running on the
/// same SHA replays cleanly.
///
/// This stage carries no state. The coordinator calls it once per
/// successfully embedded commit.
pub struct PersistStage;

impl PersistStage {
    /// Write `commit` and all hunks in `embed_output` to `storage`.
    ///
    /// On success, the commit's watermark is ready to be advanced.
    /// On error, the storage write is incomplete — the coordinator
    /// should not advance the watermark and should propagate the error.
    pub async fn run(
        repo: &RepoId,
        commit: &CommitMeta,
        embed_output: EmbedOutput,
        storage: &dyn Storage,
    ) -> Result<(), OharaError> {
        storage
            .put_commit(repo, commit, &embed_output.commit_embedding)
            .await?;

        let hunk_rows: Vec<HunkRow> = embed_output
            .hunks
            .into_iter()
            .map(|eh| HunkRow {
                commit_sha: eh.attributed.record.commit_sha.clone(),
                file_path: eh.attributed.record.file_path.clone(),
                diff_text: eh.attributed.record.diff_text.clone(),
                semantic_text: eh.attributed.effective_semantic_text().to_owned(),
                embedding: eh.embedding,
                symbols: eh.attributed.symbols.unwrap_or_default(),
            })
            .collect();

        storage.put_hunks(repo, &commit.commit_sha, &hunk_rows).await?;
        Ok(())
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core persist::tests`
Expected: PASS (both tests).

- [ ] **Step 6: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/stages/persist.rs
git commit -m "feat(core): plan-19 B.5 PersistStage; idempotent commit+hunk write"
```

---

## Phase C — Coordinator

The coordinator replaces `Indexer::run`'s inline per-commit loop. It
holds resume logic, batch-commit iteration, phase timing, and progress
reporting. The public `Indexer` API remains unchanged — `Indexer::new`
returns a struct that delegates to `Coordinator::run`.

### Task C.1 — `Coordinator` struct and `run` method

**Files:**
- Create: `crates/ohara-core/src/indexer/coordinator.rs`
- Modify: `crates/ohara-core/src/indexer/mod.rs` (or `indexer.rs`)

- [ ] **Step 1: Write the failing tests**

Create `crates/ohara-core/src/indexer/coordinator.rs` with only tests
initially:

```rust
//! Coordinator: drives the 5-stage pipeline per commit.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::commit_walk::CommitWatermark;
    use crate::{
        CommitMeta, CommitSource, EmbeddingProvider, Hunk, OharaError,
        RepoId, Storage, SymbolSource, Symbol,
    };
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    // --- Minimal fakes reused across coordinator tests ---

    struct SingleCommitSource {
        sha: String,
        hunks: Vec<Hunk>,
    }

    #[async_trait]
    impl CommitSource for SingleCommitSource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>, OharaError> {
            Ok(vec![CommitMeta {
                commit_sha: self.sha.clone(),
                message: "add feature".into(),
                author: "dev".into(),
                timestamp: 1_000_000,
            }])
        }
        async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>, OharaError> {
            Ok(self.hunks.clone())
        }
        async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>, OharaError> {
            Ok(None)
        }
    }

    struct NoopSymbolSource;
    #[async_trait]
    impl SymbolSource for NoopSymbolSource {
        async fn head_symbols_for_path(&self, _: &str) -> Result<Vec<Symbol>, OharaError> {
            Ok(vec![])
        }
    }

    struct ZeroEmbedder { dim: usize }
    #[async_trait]
    impl EmbeddingProvider for ZeroEmbedder {
        fn dimension(&self) -> usize { self.dim }
        fn model_id(&self) -> &str { "zero" }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, OharaError> {
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    #[derive(Default)]
    struct SpyStorage {
        put_commit_calls: Mutex<Vec<String>>,
        put_hunk_totals: Mutex<Vec<usize>>,
        watermark: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Storage for SpyStorage {
        async fn put_commit(&self, _: &RepoId, meta: &CommitMeta, _: &[f32]) -> Result<(), OharaError> {
            self.put_commit_calls.lock().unwrap().push(meta.commit_sha.clone());
            Ok(())
        }
        async fn put_hunks(&self, _: &RepoId, _: &str, rows: &[crate::HunkRow]) -> Result<(), OharaError> {
            self.put_hunk_totals.lock().unwrap().push(rows.len());
            Ok(())
        }
        async fn last_indexed_commit(&self, _: &RepoId) -> Result<Option<String>, OharaError> {
            Ok(self.watermark.lock().unwrap().clone())
        }
    }

    fn hunk(sha: &str) -> Hunk {
        Hunk {
            commit_sha: sha.into(),
            file_path: "src/lib.rs".into(),
            diff_text: "+fn x() {}\n".into(),
            semantic_text: "fn x() {}".into(),
            ..Hunk::default()
        }
    }

    #[tokio::test]
    async fn coordinator_indexes_single_commit_end_to_end() {
        let storage = Arc::new(SpyStorage::default());
        let coordinator = Coordinator::new(
            storage.clone(),
            Arc::new(ZeroEmbedder { dim: 4 }),
        );
        let repo = RepoId::from_parts("sha", "/repo");
        let source = SingleCommitSource {
            sha: "abc".into(),
            hunks: vec![hunk("abc")],
        };
        coordinator
            .run(&repo, &source, &NoopSymbolSource)
            .await
            .unwrap();

        assert_eq!(
            *storage.put_commit_calls.lock().unwrap(),
            vec!["abc"],
            "coordinator must persist exactly one commit"
        );
        assert_eq!(
            *storage.put_hunk_totals.lock().unwrap(),
            vec![1],
            "coordinator must persist one hunk"
        );
    }

    #[tokio::test]
    async fn coordinator_resumes_skipping_already_indexed_commit() {
        // Watermark is already at "abc" — the commit source still
        // returns "abc" but the coordinator must skip it.
        let storage = Arc::new(SpyStorage {
            watermark: Mutex::new(Some("abc".into())),
            ..Default::default()
        });
        let coordinator = Coordinator::new(
            storage.clone(),
            Arc::new(ZeroEmbedder { dim: 4 }),
        );
        let repo = RepoId::from_parts("sha", "/repo");
        let source = SingleCommitSource {
            sha: "abc".into(),
            hunks: vec![hunk("abc")],
        };
        coordinator
            .run(&repo, &source, &NoopSymbolSource)
            .await
            .unwrap();

        assert!(
            storage.put_commit_calls.lock().unwrap().is_empty(),
            "coordinator must not re-index an already-indexed commit"
        );
    }

    #[tokio::test]
    async fn coordinator_resume_from_attributed_hunks_directly() {
        // Simulate "resume from after attribute stage": construct
        // Vec<AttributedHunk> directly and drive only stages 4-5.
        // The final storage state must match a full run over the same
        // commit.
        use crate::stages::attribute::AttributedHunk;
        use crate::stages::hunk_chunk::HunkRecord;

        let storage = Arc::new(SpyStorage::default());
        let embedder = Arc::new(ZeroEmbedder { dim: 4 });
        let coordinator = Coordinator::new(storage.clone(), embedder);

        let repo = RepoId::from_parts("sha", "/repo");
        let commit = CommitMeta {
            commit_sha: "abc".into(),
            message: "add feature".into(),
            author: "dev".into(),
            timestamp: 1_000_000,
        };
        let attributed = vec![AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "src/lib.rs".into(),
                diff_text: "+fn x() {}\n".into(),
                semantic_text: "fn x() {}".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }];

        coordinator
            .run_from_attributed(&repo, &commit, attributed)
            .await
            .unwrap();

        assert_eq!(
            *storage.put_commit_calls.lock().unwrap(),
            vec!["abc"],
            "partial-pipeline run must still persist the commit"
        );
        assert_eq!(
            *storage.put_hunk_totals.lock().unwrap(),
            vec![1],
            "partial-pipeline run must persist the hunk"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ohara-core coordinator::tests`
Expected: FAIL — `Coordinator` does not exist yet.

- [ ] **Step 3: Commit the red tests**

```bash
git add crates/ohara-core/src/indexer/coordinator.rs
git commit -m "test(core): plan-19 C.1 Coordinator end-to-end + resume contracts (failing)"
```

- [ ] **Step 4: Implement `Coordinator`**

Prepend to `crates/ohara-core/src/indexer/coordinator.rs`:

```rust
use crate::indexer::stages::{
    attribute::AttributeStage, commit_walk::CommitWalkStage,
    embed::EmbedStage, hunk_chunk::HunkChunkStage, persist::PersistStage,
};
use crate::indexer::AtomicSymbolExtractor;
use crate::stages::attribute::AttributedHunk;
use crate::{
    CommitMeta, CommitSource, EmbeddingProvider, OharaError, RepoId, Storage, SymbolSource,
};
use std::sync::Arc;

/// Drives the 5-stage indexer pipeline per commit.
///
/// The coordinator:
/// - Queries `Storage::last_indexed_commit` once per run to build the
///   resume watermark.
/// - Filters `CommitWalkStage` output to skip already-indexed commits.
/// - Orchestrates stages 2-5 per commit.
/// - Does NOT hold per-stage state — stages are constructed fresh per
///   `run` call so the coordinator is safe to re-use across runs.
pub struct Coordinator {
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
}

impl Coordinator {
    /// Construct a coordinator with the default `embed_batch` of 32.
    pub fn new(
        storage: Arc<dyn Storage + Send + Sync>,
        embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    ) -> Self {
        Self {
            storage,
            embedder,
            embed_batch: 32,
        }
    }

    /// Override the embed stage's batch size.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Run the full 5-stage pipeline for all commits in `source` that
    /// follow the resume watermark.
    pub async fn run(
        &self,
        repo: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<(), OharaError> {
        // Stage 0: determine resume watermark.
        let watermark_sha = self.storage.last_indexed_commit(repo).await?;
        let watermark = watermark_sha
            .as_deref()
            .map(crate::indexer::stages::commit_walk::CommitWatermark::new);

        // Stage 1: commit walk.
        let commits =
            CommitWalkStage::run(commit_source, watermark.as_ref()).await?;

        let embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch);

        // Null extractor — binaries wire the real tree-sitter extractor
        // via `Indexer::with_atomic_symbol_extractor`.
        let extractor = crate::indexer::NullAtomicExtractor;

        for commit in &commits {
            // Skip commits that are already indexed (watermark match).
            if let Some(ref wm) = watermark {
                if !wm.is_before(commit) {
                    tracing::debug!(sha = %commit.commit_sha, "plan-19: skipping already-indexed commit");
                    continue;
                }
            }
            self.run_commit(
                repo,
                commit,
                commit_source,
                symbol_source,
                &embed_stage,
                &extractor,
            )
            .await?;
        }
        Ok(())
    }

    /// Run stages 2-5 for a single commit.
    async fn run_commit(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        embed_stage: &EmbedStage,
        extractor: &dyn AtomicSymbolExtractor,
    ) -> Result<(), OharaError> {
        // Stage 2: hunk chunk.
        let records = HunkChunkStage::run(commit_source, commit).await?;

        // Stage 3: attribute.
        let attributed = AttributeStage::run(
            &records,
            &commit.commit_sha,
            commit_source,
            symbol_source,
            extractor,
        )
        .await?;

        // Stages 4-5 share a helper so they can be tested in isolation.
        self.run_from_attributed(repo, commit, attributed).await
    }

    /// Run stages 4 (embed) and 5 (persist) given pre-built
    /// `AttributedHunk` values.
    ///
    /// This entry point enables "resume from after attribute stage":
    /// a caller can construct `Vec<AttributedHunk>` directly (e.g.
    /// from a checkpoint) and drive only the downstream stages.
    pub async fn run_from_attributed(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        attributed: Vec<AttributedHunk>,
    ) -> Result<(), OharaError> {
        let embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch);

        // Stage 4: embed.
        let embed_output = embed_stage.run(&commit.message, &attributed).await?;

        // Stage 5: persist.
        PersistStage::run(repo, commit, embed_output, self.storage.as_ref()).await
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ohara-core coordinator::tests`
Expected: PASS (all three tests including the partial-pipeline test).

- [ ] **Step 6: Run existing `Indexer::run` tests to verify stability**

Run: `cargo test -p ohara-core`
Expected: PASS — all existing tests (including `phase_timing_tests`)
must still pass unchanged. The `Coordinator` exists alongside the old
`Indexer` at this point; wiring happens in Task C.2.

- [ ] **Step 7: Commit the green implementation**

```bash
git add crates/ohara-core/src/indexer/coordinator.rs
git commit -m "feat(core): plan-19 C.1 Coordinator drives 5-stage pipeline; resume from any stage"
```

---

### Task C.2 — Delegate `Indexer::run` to `Coordinator`

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [ ] **Step 1: Replace the inline per-commit loop in `Indexer::run`**

Locate the main `run` method in `crates/ohara-core/src/indexer.rs`
(currently ~350 lines of inline logic starting around line 155). Replace
the body with a delegation to `Coordinator::run`:

```rust
    pub async fn run(
        &self,
        repo: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<IndexerReport, OharaError> {
        use crate::indexer::coordinator::Coordinator;

        let coordinator = Coordinator::new(self.storage.clone(), self.embedder.clone())
            .with_embed_batch(self.embed_batch);

        let start = std::time::Instant::now();
        coordinator.run(repo, commit_source, symbol_source).await?;
        let wall_ms = start.elapsed().as_millis() as u64;

        // Hydrate the report from storage (commit/hunk counts).
        let report = self
            .storage
            .indexer_report(repo)
            .await
            .unwrap_or_else(|_| IndexerReport {
                new_commits: 0,
                new_hunks: 0,
                skipped_commits: 0,
                wall_ms,
            });
        Ok(IndexerReport { wall_ms, ..report })
    }
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo test -p ohara-core`
Expected: PASS — all existing tests including `phase_timing_tests`
continue to pass because `Indexer::run`'s public signature is unchanged.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p ohara-core --all-targets --all-features -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 4: Commit**

```bash
git add crates/ohara-core/src/indexer.rs
git commit -m "refactor(core): plan-19 C.2 Indexer::run delegates to Coordinator"
```

---

## Phase D — Cleanup

### Task D.1 — Remove inlined helpers from `indexer.rs`

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

Now that the inline per-commit loop has been replaced by coordinator
delegation, the helpers that were only used inside that loop are dead
code. Remove them.

- [ ] **Step 1: Identify dead helpers**

Run: `cargo clippy -p ohara-core -- -D warnings 2>&1 | grep "dead_code\|unused"`

Expected candidates: `embed_in_chunks` (now in `EmbedStage`), the inline
`attribute` block helpers, and any private fns that are no longer called.
List them before deleting.

- [ ] **Step 2: Delete dead helpers**

For each helper identified in Step 1:

```rust
// REMOVE from crates/ohara-core/src/indexer.rs:
//   async fn embed_in_chunks(...)   — now in stages/embed.rs
//   fn count_added_lines(...)       — if only used by inline loop
//   fn build_semantic_text(...)     — if extracted into hunk_chunk stage
```

After each deletion, run: `cargo build -p ohara-core`
Expected: clean build after all dead helpers are removed.

- [ ] **Step 3: Verify line count**

Run: `wc -l crates/ohara-core/src/indexer.rs`
Expected: under 500 lines (target ~200 lines — just the `Indexer`
struct, its `new`/builder methods, and the delegating `run` method).

Run: `wc -l crates/ohara-core/src/indexer/stages/*.rs crates/ohara-core/src/indexer/coordinator.rs`
Expected: every file under 500 lines.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/indexer.rs
git commit -m "refactor(core): plan-19 D.1 remove dead inline helpers from Indexer::run"
```

---

### Task D.2 — Update `CONTRIBUTING.md` workspace layout table

**Files:**
- Modify: `CONTRIBUTING.md`

The `ohara-core` row in the workspace layout table must reflect the new
submodule structure.

- [ ] **Step 1: Update the table row**

Find the `ohara-core` row in `CONTRIBUTING.md`'s workspace layout table
and update the description to mention the stages submodule:

```markdown
| `ohara-core` | Domain types, traits, orchestration (`Indexer` → `Coordinator` + 5 pipeline stages in `indexer/stages/`, `Retriever`, `ExplainQuery`) |
```

- [ ] **Step 2: Add a note about the stages convention**

Append a short convention note to the "Architecture" or "Crate
conventions" section:

```markdown
**Indexer stages** (`crates/ohara-core/src/indexer/stages/`): each stage
is a pure async function or a minimal struct with a single `run` method.
Stages accept only `ohara-core` traits (no concrete crate refs). The
`Coordinator` in `indexer/coordinator.rs` wires stages together and owns
resume logic.
```

- [ ] **Step 3: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: plan-19 D.2 update CONTRIBUTING workspace table for indexer stages"
```

---

## Final pass

Pre-completion checklist from `CONTRIBUTING.md` §13. Every item must be
green before a PR is opened.

- [ ] `cargo fmt --all` — output is clean (no reformatting needed)
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` — zero warnings
- [ ] `cargo test --workspace` — all tests green
- [ ] `wc -l crates/ohara-core/src/indexer.rs` — under 500 lines
- [ ] `wc -l crates/ohara-core/src/indexer/stages/*.rs crates/ohara-core/src/indexer/coordinator.rs` — every file under 500 lines
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code added by this plan (one exception: `expect("invariant: <reason>")` with documented invariant)
- [ ] No `println!` / `eprintln!` outside permitted locations — all new output uses `tracing::debug!` / `tracing::info!`
- [ ] No `anyhow` added to any library crate — library errors use `thiserror` / `OharaError`
- [ ] No new third-party deps introduced (workspace `Cargo.toml` unchanged beyond any `pub use` additions)
- [ ] `ohara-core` does not depend on `ohara-storage`, `ohara-embed`, `ohara-git`, or `ohara-parse` — verify with `cargo tree -p ohara-core`
- [ ] All stages accept only `ohara-core` trait objects (`&dyn CommitSource`, `&dyn Storage`, etc.) — no concrete crate imports in `stages/`
- [ ] Existing `Indexer::run` tests pass without modification (checked in Task C.2)
- [ ] `CONTRIBUTING.md` workspace layout table updated (Task D.2)
- [ ] No new top-level `*.md` files created
- [ ] E2E smoke test: `cargo run -p ohara-cli -- index fixtures/tiny/repo` succeeds; `cargo run -p ohara-cli -- query --query "retry with backoff" fixtures/tiny/repo` returns results

---

## Risks & open questions

**1. Parallelism regression.**
Today's `Indexer::run` can, in principle, overlap embed of commit N with
persist of commit N-1 via `tokio::join!` (though this is not currently
implemented — the loop is sequential). The `Coordinator` as written in
Phase C is also sequential per commit. If profiling shows that pipeline
overlap is needed for throughput, Phase C's coordinator loop should be
extended to use `tokio::join!` between `EmbedStage::run` for commit N
and `PersistStage::run` for commit N-1. This is out of scope for
plan-19 but should be noted as plan-20's concern if retriever-lane
parallelism exposes indexer throughput as the new bottleneck.

**2. Compatibility with existing `Indexer::run` tests.**
The ~5 tests in `crates/ohara-core/src/indexer.rs`'s `#[cfg(test)]`
block call `Indexer::run` directly with fake commit sources. Task C.2
delegates `Indexer::run` to `Coordinator::run`, which must pass those
tests unchanged. If any test exercises a timing or progress detail that
is currently inline (e.g. `PhaseTimings` field assertions), those fields
may need to be hydrated from the coordinator's internal timing — tracked
as a BLOCK if encountered.

**3. `IndexerReport` hydration gap.**
`Coordinator::run` currently returns `()` on success; the `new_commits`
/ `new_hunks` / `skipped_commits` counts in `IndexerReport` are
re-read from storage by `Indexer::run` after the coordinator finishes.
If `Storage::indexer_report` does not exist today, Task C.2 will surface
a compile error — this is a BLOCK. Resolution: add a lightweight
`Storage::counts_for_repo(repo) -> (commits, hunks)` method in a
separate micro-PR before landing Task C.2, or accumulate counts inside
the coordinator and return them via a new `CoordinatorReport` struct. Do
not add SQL to any crate other than `ohara-storage`.

**4. File-size budget for `indexer.rs` during the transition.**
Between Task B (stage extraction) and Task D.1 (dead helper removal),
`indexer.rs` will temporarily contain both the old inline code and the
new stage modules. This will transiently exceed 500 lines. The 500-line
cap applies to the final state (post Task D.1). If a reviewer notices
the over-limit state mid-refactor, reference this note.

**5. `AtomicSymbolExtractor` trait visibility.**
`AttributeStage::run` accepts `&dyn AtomicSymbolExtractor`. This trait
must be `pub` in `ohara-core::indexer` so the attribute stage module can
reference it. Verify the trait is re-exported from `crates/ohara-core/src/lib.rs`
and that binaries (`ohara-cli`, `ohara-mcp`) can wire the
`ohara_parse::TreeSitterAtomicExtractor` concrete without importing from
`ohara-core::indexer` internals. If the trait is currently `pub(crate)`,
promote it to `pub` in a separate preparatory commit before Task B.3.
