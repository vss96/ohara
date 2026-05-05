# ohara plan-22 — Recency formula: handle negative rerank scores

> **Status:** draft

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each green
> implementation.

**Goal:** fix a latent ranking bug in
`ohara_core::retriever::Retriever::find_pattern_with_profile` where the
multiplicative recency formula `combined = rerank * (1.0 + α * recency)`
inverts ordering when the cross-encoder returns a negative logit. With
`bge-reranker-base` (the only `RerankProvider` impl in tree), raw scores
for poor matches are routinely negative. Multiplying a negative base by a
positive `(1 + α·recency)` makes it *more* negative — so an older
bad-match (low recency factor) ends up with a *less* negative combined
score and outranks a newer bad-match.

**Architecture:** apply a numerically-stable sigmoid to the raw rerank
logit before combining with recency. Sigmoid maps the logit into `(0, 1)`,
restoring the invariant that "more recent ⇒ higher combined score" for
every pair of hits regardless of rerank sign. Existing
`PatternHit.combined_score` field is preserved (callers — CLI, MCP — read
it for display ordering only). No public API change.

**Tech Stack:** Rust 2021, existing `crate::retriever`, no new crates.

**Spec:** none — bug-fix; the original design doc
(`docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md`)
describes the recency multiplier abstractly without specifying score
domain.

**Scope check:** plan-22 is `ohara-core` only. No SQL changes, no storage
trait changes, no embed-crate changes, no MCP/CLI behavior change beyond
the corrected ordering. Existing tests pass after the fix.

---

## Phase A — Reproduce the bug with a failing test

### Task A.1 — Failing test: negative rerank score inverts recency ordering

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs` (add test in existing
  `#[cfg(test)] mod tests` block, alongside
  `find_pattern_recency_multiplier_breaks_ties_when_no_rerank`)

- [ ] **Step 1: Write the failing test**

Append the following test inside the existing `mod tests` block in
`crates/ohara-core/src/retriever.rs`. The test sets up two hits — one
recent, one a year old — and a `ScriptedReranker` that returns a
negative logit for both (simulating poor matches per cross-encoder, which
still need to be ranked relative to each other). The recent hit MUST
outrank the older hit; the current code does the opposite.

```rust
#[tokio::test]
async fn find_pattern_negative_rerank_still_ranks_recent_above_old() {
    // Regression for plan-22: when the cross-encoder returns negative
    // logits for low-relevance candidates (which `bge-reranker-base`
    // routinely does), the multiplicative recency formula must still
    // place the more recent hit above the older one. Pre-fix the
    // ordering inverts because `negative * (1 + small_positive)` is
    // *more* negative than `negative * (1 + larger_positive)`.
    let now = 1_700_000_000_i64;
    let day = 86_400_i64;

    // Both candidates land in disjoint single-element lanes (RRF rank 1
    // in their lane, absent from the others) so RRF gives them equal
    // fused scores and ordering is dictated entirely by
    // `combined = f(rerank, recency)`.
    let knn = vec![fake_hit(1, "old", now - 365 * day, 0.5, "diff-bad-old")];
    let fts_text = vec![fake_hit(2, "new", now - day, 0.5, "diff-bad-new")];
    let storage = Arc::new(FakeStorage::new(knn, fts_text, vec![]));
    let embedder = Arc::new(FakeEmbedder);

    // Reranker assigns the *same* negative logit to both candidates.
    // Under the pre-fix multiplicative formula this is the worst case:
    // identical bases, only the recency multiplier differentiates, and
    // it differentiates in the wrong direction.
    let scores: HashMap<String, f32> = HashMap::from([
        ("diff-bad-old".to_string(), -2.0),
        ("diff-bad-new".to_string(), -2.0),
    ]);
    let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker { scores });

    let r = Retriever::new(storage, embedder).with_reranker(reranker);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let id = RepoId::from_parts("x", "/y");
    let out = r.find_pattern(&id, &q, now).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0].commit_sha, "new",
        "newer commit MUST outrank older when rerank scores are tied,\
         even when both are negative; got order {:?}",
        out.iter().map(|h| h.commit_sha.as_str()).collect::<Vec<_>>()
    );
    assert!(
        out[0].combined_score > out[1].combined_score,
        "combined_score must be monotone with sort order; got new={} old={}",
        out[0].combined_score, out[1].combined_score
    );
}
```

- [ ] **Step 2: Run the test and confirm it fails**

Run: `cargo test -p ohara-core --lib find_pattern_negative_rerank_still_ranks_recent_above_old -- --nocapture`

Expected output: test fails with assertion `newer commit MUST
outrank older when rerank scores are tied, even when both are
negative; got order ["old", "new"]`.

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "test(retriever): add failing regression for negative-rerank recency ordering"
```

---

## Phase B — Fix: sigmoid-normalize the rerank logit

### Task B.1 — Add `sigmoid` helper and apply it inside `find_pattern_with_profile`

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs:300-345` (the recency-application block)

- [ ] **Step 1: Add a private `sigmoid` helper near the top of the impl block**

Insert this free function above `impl Retriever` in
`crates/ohara-core/src/retriever.rs` (anywhere before the impl is fine;
above the `pub struct Retriever` declaration is the cleanest spot):

```rust
/// Numerically-stable logistic sigmoid, mapping `(-∞, +∞) → (0, 1)`.
///
/// Used to bound the cross-encoder's raw logit so the multiplicative
/// recency factor in `find_pattern_with_profile` always boosts in the
/// expected direction (more recent ⇒ higher combined score). The
/// branch on `x.is_sign_positive()` avoids `exp` overflow for large-
/// magnitude inputs in either direction.
fn sigmoid(x: f32) -> f32 {
    if x.is_sign_positive() {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}
```

- [ ] **Step 2: Apply `sigmoid` in the combine step**

Locate the `out: Vec<PatternHit>` construction in
`find_pattern_with_profile` (currently at
`crates/ohara-core/src/retriever.rs:308-339`). Replace the
`combined = s * (1.0 + effective_recency_weight * recency)` line and
its surrounding closure body so the rerank score is sigmoid-normalised
*before* the multiplicative recency factor is applied. The full
replacement closure (preserves every other field unchanged):

```rust
let mut out: Vec<PatternHit> = hits
    .into_iter()
    .zip(rerank_scores)
    .map(|(h, s)| {
        let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86400.0;
        let recency = (-age_days / effective_weights.recency_half_life_days).exp();
        // plan-22: sigmoid the rerank logit so the multiplicative
        // recency factor preserves "newer ⇒ higher combined score"
        // for every score sign. The cross-encoder (`bge-reranker-base`)
        // emits raw, signed logits; without the sigmoid, two equally-
        // bad candidates would order older-above-newer because
        // `negative * (1 + small)` is less negative than `negative *
        // (1 + larger)`. The degraded-mode constant 1.0 fed in by
        // `no_rerank` paths sigmoids to ~0.731, which keeps the
        // recency tie-breaker behaviour identical relative to other
        // 1.0-scored peers (every candidate scales by the same factor)
        // and the existing recency-only ordering test still passes.
        let s_norm = sigmoid(s);
        let combined = s_norm * (1.0 + effective_recency_weight * recency);
        // Bogus ts (out-of-range i64) falls back to "" — PatternHit.commit_date
        // is informational, not a contract, so an empty string is acceptable.
        let date = DateTime::<Utc>::from_timestamp(h.commit.ts, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();
        let (excerpt, truncated) = truncate_diff(&h.hunk.diff_text, DIFF_EXCERPT_MAX_LINES);
        let related_head_symbols =
            symbols_by_hunk.get(&h.hunk_id).cloned().unwrap_or_default();
        PatternHit {
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
            recency_weight: recency,
            combined_score: combined,
            provenance: Provenance::Inferred,
        }
    })
    .collect();
```

- [ ] **Step 3: Update the doc comment on `RankingWeights::recency_weight`**

Replace the existing doc comment on `RankingWeights::recency_weight`
(currently `crates/ohara-core/src/retriever.rs:22-26`) with the
following — it describes the post-fix formula accurately so the next
reader doesn't get blindsided by the sigmoid:

```rust
/// Multiplier on the recency factor in the final score:
/// `final = sigmoid(rerank) * (1.0 + recency_weight * exp(-age_days / half_life_days))`.
///
/// `sigmoid(rerank)` bounds the cross-encoder's signed logit into
/// `(0, 1)` so the multiplicative recency factor always boosts in the
/// expected direction (more recent ⇒ higher combined score). See
/// plan-22 for the bug this fixed.
///
/// Default 0.05 — small enough to act as a tie-breaker without
/// overpowering rerank quality.
pub recency_weight: f32,
```

- [ ] **Step 4: Run the regression test and confirm it passes**

Run: `cargo test -p ohara-core --lib find_pattern_negative_rerank_still_ranks_recent_above_old -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Run the surrounding retriever tests to confirm no regressions**

Run: `cargo test -p ohara-core --lib retriever::tests`

Expected: all tests pass — including
`find_pattern_recency_multiplier_breaks_ties_when_no_rerank`
(degraded-mode 1.0 sigmoids to ~0.731, same scaling for every peer,
recency tie-breaker semantics unchanged) and
`profile_recency_half_life_30_shrinks_recency_factor_for_old_commits`
(asserts `recency_weight` field, which is the unscaled recency
factor, not the combined score — unchanged by the sigmoid).

- [ ] **Step 6: Commit the fix**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "fix(retriever): sigmoid-normalise rerank logit before recency multiplier

Pre-fix, two equally-bad candidates with the same negative cross-
encoder logit would order older-above-newer because
\`negative * (1 + small)\` is less negative than
\`negative * (1 + larger)\`. Apply a numerically-stable sigmoid to
the raw rerank score before the multiplicative recency factor so
\"newer ⇒ higher combined score\" holds for every score sign.
Degraded-mode (no rerank, score=1.0) sigmoids to ~0.731, identical
scaling for every peer, so existing recency-only ordering is
unchanged."
```

---

## Phase C — Document the score domain in `embed.rs`

The bug originated from an implicit assumption about the score range.
Lock the contract in writing so future reranker implementations don't
re-introduce the same hazard.

### Task C.1 — Document the rerank score domain on the trait

**Files:**
- Modify: `crates/ohara-core/src/embed.rs:14-24`

- [ ] **Step 1: Replace the doc comment on `RerankProvider`**

Replace the existing `RerankProvider` doc comment with:

```rust
/// Cross-encoder reranker contract.
///
/// Score `candidates` against `query`. Output length == `candidates.len()`;
/// element `i` is the score for `candidates[i]`. Higher is better.
///
/// **Score domain:** unbounded `f32`. Implementations MAY return raw
/// cross-encoder logits (the `fastembed::TextRerank` impl in
/// `ohara-embed` does), which means scores CAN be negative for
/// low-relevance pairs. Downstream consumers in `crate::retriever`
/// sigmoid-normalise the score before any multiplicative combination
/// (see plan-22). New implementations MUST NOT silently apply their
/// own normalisation that would clamp the score into `[0, 1]`, since
/// that would lose the relative-ordering signal cross-encoders
/// produce in the negative range.
///
/// Implementations MUST be order-preserving with respect to the input
/// slice (i.e. the returned `Vec<f32>` aligns positionally with
/// `candidates`).
#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
}
```

- [ ] **Step 2: Run the embed-crate tests to confirm nothing else broke**

Run: `cargo test -p ohara-core --lib embed::`

Expected: PASS.

- [ ] **Step 3: Run `cargo fmt` and `cargo clippy` to satisfy the gates**

Run: `cargo fmt --all`
Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: clippy clean.

- [ ] **Step 4: Commit the doc-comment change**

```bash
git add crates/ohara-core/src/embed.rs
git commit -m "docs(embed): document rerank score domain (signed logits, not [0,1])"
```

---

## Phase D — Final gate

### Task D.1 — Full workspace test suite

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`

Expected: all tests pass. If any retriever or pipeline test fails,
inspect — the sigmoid changes the *absolute* `combined_score` value
but not the *relative* ordering of any given hit-set with consistent
score signs, so any failure is informative.

- [ ] **Step 2: Confirm `cargo fmt` and `cargo clippy` are clean**

Run: `cargo fmt --all -- --check`
Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: both pass.

- [ ] **Step 3: Update the plan status header**

Edit `docs/superpowers/plans/2026-05-05-ohara-plan-22-recency-formula-negative-rerank-fix.md`
and change `> **Status:** draft` to `> **Status:** complete`.

- [ ] **Step 4: Commit the plan-status update**

```bash
git add docs/superpowers/plans/2026-05-05-ohara-plan-22-recency-formula-negative-rerank-fix.md
git commit -m "docs(plan-22): mark complete"
```
