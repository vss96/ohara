# ohara plan-25 — Wire contextual BM25 lane into the retriever

> **Status:** complete

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each green
> implementation.

**Goal:** wire the existing `Storage::bm25_hunks_by_semantic_text`
method (added in plan-11, never reached the retriever) into
`Retriever::find_pattern_with_profile` as a 4th retrieval lane fused
through the existing RRF + cross-encoder rerank pipeline. Closes the
plan-11-Task-4.1 follow-up flagged in the test-fake comment at
`crates/ohara-core/src/retriever.rs:491`.

**Architecture:** see
`docs/superpowers/specs/2026-05-05-ohara-contextual-bm25-wiring-design.md`.
TL;DR: add a 4th lane (don't replace the raw-`diff_text` lane), gate
it behind a new `RetrievalProfile::semantic_text_lane_enabled` flag
(default `true` for every profile), validate via the plan-10
context-engine eval.

**Tech Stack:** Rust 2021, no new crates, no SQL migration (V4
already deployed `fts_hunk_semantic`).

**Spec:** `docs/superpowers/specs/2026-05-05-ohara-contextual-bm25-wiring-design.md`

**Scope check:** plan-25 touches `ohara-core` (retriever +
query_understanding profile struct) only. No storage trait changes
(method already exists). No CLI / MCP behaviour change beyond
recall improvements observable through the eval.

---

## Phase A — Profile flag

### Task A.1 — Add `semantic_text_lane_enabled` to `RetrievalProfile`

**Files:**
- Modify: `crates/ohara-core/src/query_understanding.rs:87-170` (struct
  field, every per-intent constructor, `default_unknown`)

- [ ] **Step 1: Failing test — every profile defaults the new flag to true**

Append to the existing `mod tests` block of `query_understanding.rs`:

```rust
#[test]
fn every_profile_enables_semantic_text_lane_by_default() {
    // Plan 25: the new lane ships enabled on every profile so that the
    // plan-10 eval measures the win unconditionally. Profile-specific
    // disablement is a follow-up once we have eval data per intent.
    use crate::query_understanding::QueryIntent;
    for intent in [
        QueryIntent::Unknown,
        QueryIntent::BugFixPrecedent,
        QueryIntent::ApiUsage,
        QueryIntent::Configuration,
        QueryIntent::ImplementationPattern,
    ] {
        let p = RetrievalProfile::for_intent(intent);
        assert!(
            p.semantic_text_lane_enabled,
            "{intent:?} must enable the semantic-text lane",
        );
    }
}
```

- [ ] **Step 2: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib every_profile_enables_semantic_text_lane_by_default`

Expected: fails — the field doesn't exist yet, so the test won't even
compile. The compilation error is the failure signal.

- [ ] **Step 3: Add the field to `RetrievalProfile`**

In `crates/ohara-core/src/query_understanding.rs`, add to the
`RetrievalProfile` struct (alongside the existing `vec_lane_enabled`
/ `text_lane_enabled` / `symbol_lane_enabled` flags):

```rust
    /// Plan 25: enables the BM25 lane over `hunk.semantic_text` (the
    /// contextual preamble + added-lines blob produced at index time
    /// by `hunk_text::build`). Complements `text_lane_enabled` (raw
    /// `diff_text`); both can be on at the same time, fused via RRF.
    pub semantic_text_lane_enabled: bool,
```

- [ ] **Step 4: Initialize the field in every constructor**

Find every `RetrievalProfile { ... }` literal in
`query_understanding.rs` (use `grep -n "RetrievalProfile {" crates/ohara-core/src/query_understanding.rs`)
and add `semantic_text_lane_enabled: true,` next to the existing
`text_lane_enabled` line. Likely sites:

- `default_unknown` (around line 128)
- `bug_fix` / `api_usage` / `configuration` / `implementation_pattern`
  (any per-intent constructors that follow `default_unknown`)

If a profile sets `text_lane_enabled: false`, set the new field to
`false` too — that's the explicit "don't run any text-BM25 lane"
intent. (If no such profile exists today, every site gets `true`.)

- [ ] **Step 5: Run the test and confirm it passes**

Run: `cargo test -p ohara-core --lib every_profile_enables_semantic_text_lane_by_default`

Expected: PASS.

- [ ] **Step 6: Update the existing `default_unknown` shape test**

The existing test
`default_unknown_has_all_lanes_enabled_and_no_overrides` (around
`query_understanding.rs:574`) asserts `vec_lane_enabled &&
text_lane_enabled && symbol_lane_enabled`. Append the new lane:

```rust
    assert!(p.vec_lane_enabled && p.text_lane_enabled && p.symbol_lane_enabled
        && p.semantic_text_lane_enabled);
```

Run: `cargo test -p ohara-core --lib default_unknown_has_all_lanes_enabled_and_no_overrides`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/query_understanding.rs
git commit -m "feat(query-profile): add semantic_text_lane_enabled (default true)"
```

---

## Phase B — Wire the lane into the retriever

### Task B.1 — Failing test: 4-lane gather hits `bm25_hunks_by_semantic_text`

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (test block — extend
  `FakeStorage` to count semantic-text-lane calls + return scripted
  hits)

- [ ] **Step 1: Make `FakeStorage::bm25_hunks_by_semantic_text` script-able**

Today `FakeStorage` (around `retriever.rs:421`) hardcodes
`bm25_hunks_by_semantic_text` to return `Vec::new()` (with a
`fts_semantic` call-recording entry). Promote that to a real field
matching the existing `knn` / `fts_text` / `fts_sym` pattern:

```rust
    struct FakeStorage {
        knn: Vec<HunkHit>,
        fts_text: Vec<HunkHit>,
        fts_sym: Vec<HunkHit>,
        fts_semantic: Vec<HunkHit>,            // <-- new
        calls: Mutex<Vec<&'static str>>,
        // (whatever fields plan-24 added — keep them)
    }

    impl FakeStorage {
        fn new(
            knn: Vec<HunkHit>,
            fts_text: Vec<HunkHit>,
            fts_sym: Vec<HunkHit>,
        ) -> Self {
            Self::new_with_semantic(knn, fts_text, fts_sym, vec![])
        }

        // Plan 25: secondary constructor for tests that need to script
        // the semantic-text lane. Existing tests keep using `new(...)`.
        fn new_with_semantic(
            knn: Vec<HunkHit>,
            fts_text: Vec<HunkHit>,
            fts_sym: Vec<HunkHit>,
            fts_semantic: Vec<HunkHit>,
        ) -> Self {
            Self {
                knn,
                fts_text,
                fts_sym,
                fts_semantic,
                calls: Mutex::new(vec![]),
                // (initialize plan-24 fields here as needed)
            }
        }
    }
```

Then update the `bm25_hunks_by_semantic_text` impl on `FakeStorage`
to return `self.fts_semantic.clone()` (mirroring the other lane fakes).

- [ ] **Step 2: Add the failing regression test**

```rust
#[tokio::test]
async fn find_pattern_invokes_semantic_text_lane_and_fuses_into_rrf() {
    // Plan 25: the semantic-text lane MUST be queried, and a hit
    // surfaced ONLY by that lane MUST appear in the fused output. We
    // construct lanes so the semantic lane is the *only* source for
    // hunk_id=99; if the new lane is wired in, hunk 99 surfaces;
    // otherwise it doesn't.
    let now = 1_700_000_000;
    let knn = vec![fake_hit(1, "a", now, 0.9, "diff-a")];
    let fts_text = vec![fake_hit(2, "b", now, 0.5, "diff-b")];
    let fts_sym = vec![fake_hit(3, "c", now, 0.3, "diff-c")];
    let fts_semantic = vec![fake_hit(99, "z", now, 0.7, "diff-z-only-in-semantic")];
    let storage = Arc::new(FakeStorage::new_with_semantic(
        knn, fts_text, fts_sym, fts_semantic,
    ));
    let embedder = Arc::new(FakeEmbedder);
    let r = Retriever::new(storage.clone(), embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 10,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let out = r.find_pattern(&id, &q, now).await.unwrap();

    let calls = storage.calls.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| *c == "fts_semantic"),
        "semantic-text lane MUST be invoked; calls = {calls:?}"
    );
    assert!(
        out.iter().any(|h| h.commit_sha == "z"),
        "hunk surfaced ONLY by the semantic-text lane MUST appear in fused output; \
         got {:?}",
        out.iter().map(|h| h.commit_sha.as_str()).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 3: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib find_pattern_invokes_semantic_text_lane_and_fuses_into_rrf -- --nocapture`

Expected: fails — `bm25_hunks_by_semantic_text` is not called by the
retriever, so `calls` lacks `"fts_semantic"` and the assertion fires.

- [ ] **Step 4: Commit the failing test**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "test(retriever): assert semantic-text lane participates in RRF fusion"
```

### Task B.2 — Wire the lane into the gather block

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (the lane-gather block)

> **Coordination with plan-24:** if plan-24's lane-mask hoist (Phase C)
> already landed, the gather block is the `OptionFuture`-based variant.
> If not, it's still the original `tokio::join!` block. The diff below
> assumes the **post-plan-24** shape; if plan-24 hasn't landed yet,
> apply the equivalent change inside the existing `tokio::join!`:
> add a 5th future `sem_fut` for `bm25_hunks_by_semantic_text`,
> propagate the result through the same `profile.<flag>` filter at
> the bottom, and feed the resulting `sem_hits` into the
> `reciprocal_rank_fusion(&[ranking_vec, ranking_fts, ranking_sym, ranking_sem], 60)`
> call.

- [ ] **Step 1: Add the semantic-text lane future**

Inside `find_pattern_with_profile` (or `find_pattern_inner` if
plan-24 already extracted it), add a 5th future alongside the
existing four:

```rust
        let sem_fut: OptionFuture<_> = if profile.semantic_text_lane_enabled {
            Some(timed_phase(
                "lane_fts_semantic",
                self.storage.bm25_hunks_by_semantic_text(
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
```

Then extend the `tokio::join!` to await `sem_fut` along with the
others, and unwrap its `Option<Result<…>>` the same way:

```rust
        let (vec_opt, fts_opt, hist_sym_opt, head_sym_opt, sem_opt) =
            tokio::join!(vec_fut, fts_fut, hist_sym_fut, head_sym_fut, sem_fut);
        // …existing transposes for vec/fts/hist_sym/head_sym…
        let sem_hits: Vec<HunkHit> = sem_opt.transpose()?.unwrap_or_default();
```

> **Pre-plan-24 alternative:** add `sem_fut` to the existing
> `tokio::join!` and gate the result with the existing
> `if profile.semantic_text_lane_enabled { sem_res? } else { Vec::new() }`
> pattern (see the `vec_hits` / `fts_hits` block at
> `retriever.rs:197-215`).

- [ ] **Step 2: Add `sem_hits` to the per-id rank tables**

After the existing `ranking_vec` / `ranking_fts` / `ranking_sym`
construction (around `retriever.rs:227-245`), add:

```rust
        let mut ranking_sem: Vec<HunkId> = Vec::with_capacity(sem_hits.len());
        for h in &sem_hits {
            ranking_sem.push(h.hunk_id);
            by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
        }
```

- [ ] **Step 3: Pass the new ranking into RRF**

Replace the existing RRF call:

```rust
        let fused: Vec<HunkId> = timed_phase("rrf", async {
            reciprocal_rank_fusion(&[ranking_vec, ranking_fts, ranking_sym], 60)
        })
        .await;
```

…with:

```rust
        let fused: Vec<HunkId> = timed_phase("rrf", async {
            reciprocal_rank_fusion(
                &[ranking_vec, ranking_fts, ranking_sym, ranking_sem],
                60,
            )
        })
        .await;
```

- [ ] **Step 4: Run the failing test from Task B.1 and confirm it passes**

Run: `cargo test -p ohara-core --lib find_pattern_invokes_semantic_text_lane_and_fuses_into_rrf -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run the full retriever test suite**

Run: `cargo test -p ohara-core --lib retriever::`

Expected: every test passes.

- [ ] **Step 6: Update the phase-event capture test**

`find_pattern_emits_expected_phase_events` (around
`retriever.rs:776-817`) lists the expected phase event names. Add
`"lane_fts_semantic"` to the required list:

```rust
        for required in [
            "embed_query",
            "lane_knn",
            "lane_fts_text",
            "lane_fts_sym_hist",
            "lane_fts_sym_head",
            "lane_fts_semantic",  // <-- new
            "rrf",
            "hydrate_symbols",
        ] { ... }
```

> **Note on `embed_query`:** if plan-24 hoisted the embedding call
> inside the vec-lane future and renamed the phase to `lane_knn`-only,
> drop `"embed_query"` from the list. Match the actual emitted names.

Run: `cargo test -p ohara-core --lib find_pattern_emits_expected_phase_events`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "feat(retriever): wire bm25_hunks_by_semantic_text as 4th RRF lane

Plan 11 added Storage::bm25_hunks_by_semantic_text and the V4
fts_hunk_semantic table but never wired the retrieval side. Plan 25
closes that gap: the lane runs in parallel with the three existing
lanes, gated by the new RetrievalProfile::semantic_text_lane_enabled
flag (default true for every profile), and its hits fuse into the
existing RRF (k=60). The vec lane already operates on the same
contextual semantic_text via hunk_text::build at index time; this
change brings the BM25 side to parity, mirroring the contextual-BM25
recommendation in Anthropic's \"Contextual Retrieval\" post."
```

---

## Phase C — Eval validation

### Task C.1 — Run the plan-10 context-engine eval

- [ ] **Step 1: Run the eval**

```bash
cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture
```

Expected: stderr contains a JSON metrics line with `recall_at_5`,
`mrr`, `p50_ms`, `p95_ms`. Capture the line; you'll need to compare
it to the previous baseline.

- [ ] **Step 2: Compare against the pre-change baseline**

Find the pre-change baseline by checking git log for the most recent
context_engine_eval baseline commit:

```bash
git log --oneline -- 'tests/perf/baselines/' | head
```

Diff the captured metrics line against the baseline.

| Metric change | Action |
|---|---|
| `recall_at_5 == 1.0` AND `mrr >= baseline.mrr` AND ≥1 case improves | Proceed to Step 3. |
| `recall_at_5 < 1.0` OR `mrr` regresses | Revert Phase B's RRF change (the lane stays plumbed, but `semantic_text_lane_enabled` defaults to `false` on every profile). Add a note to the design spec under "Eval results — outcome". |

- [ ] **Step 3: Commit the new eval baseline**

If a baseline directory exists, save the new metrics line:

```bash
mkdir -p tests/perf/baselines
# overwrite (or create) tests/perf/baselines/context_engine_eval.jsonl
```

Commit:

```bash
git add tests/perf/baselines/context_engine_eval.jsonl
git commit -m "perf(eval): refresh context-engine baseline after plan-25 lane wiring"
```

> If no baselines directory exists yet, this step is a no-op — the
> eval runner already prints the line on stderr; future runs will
> compare visually.

---

## Phase D — Final gate

### Task D.1 — Workspace gate + plan status

- [ ] **Step 1: Full workspace gate**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Run: `cargo test --workspace`

Expected: all three pass.

- [ ] **Step 2: Update the design-spec status**

Edit
`docs/superpowers/specs/2026-05-05-ohara-contextual-bm25-wiring-design.md`
and change `> **Status:** draft` to `> **Status:** implemented`.

Add a one-paragraph "Outcome" section at the bottom summarizing the
eval delta from Phase C.

- [ ] **Step 3: Update the plan status**

Edit
`docs/superpowers/plans/2026-05-05-ohara-plan-25-contextual-bm25-lane.md`
and change `> **Status:** draft` to `> **Status:** complete`.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/plans/2026-05-05-ohara-plan-25-contextual-bm25-lane.md \
        docs/superpowers/specs/2026-05-05-ohara-contextual-bm25-wiring-design.md
git commit -m "docs(plan-25): mark complete + record eval outcome"
```
