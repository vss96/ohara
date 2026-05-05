# Contextual BM25 Lane Wiring — Design Note

> **Status:** draft
> **Drives:** `docs/superpowers/plans/2026-05-05-ohara-plan-25-contextual-bm25-lane.md`
> **Date:** 2026-05-05

## Background

Anthropic's "Contextual Retrieval" (https://www.anthropic.com/engineering/contextual-retrieval)
reports that prepending an LLM-generated 50–100 token contextual preamble to
each chunk before embedding **and** before BM25 indexing reduces retrieval
failure rate by **35% (embeddings only)** and **49% (combined with
contextual BM25)**.

A literal application of that recipe to ohara would be expensive
(`Claude 3 Haiku` per hunk × thousands of commits × every reindex), but
git already supplies the natural "preamble": commit message, file path,
language, touched-symbol names, and change-kind. Plan 11 already
implemented exactly that path on the **embedding side**:

- `crates/ohara-core/src/hunk_text.rs::build` produces:
  ```text
  commit: <message first line>
  file:   <file path>
  language: <lang>
  symbols: <touched symbol names, comma-joined>
  change: <added | modified | deleted | renamed>
  added_lines:
  <every '+'-prefixed body line, '+' stripped>
  ```
- `crates/ohara-core/src/indexer.rs:311-322` calls `hunk_text::build` per
  hunk and feeds the result into `embed_in_chunks` (so the **vector lane
  already operates on contextual embeddings**).
- The V4 migration adds a `hunk.semantic_text` column and an
  `fts_hunk_semantic` FTS5 table, with a backfill that seeds
  `semantic_text = diff_text` for pre-V4 rows.
- `Storage::bm25_hunks_by_semantic_text` was added in plan-11 Task ?, ready
  to query the FTS5 table.

## The gap

`Retriever::find_pattern_with_profile` in
`crates/ohara-core/src/retriever.rs:155-225` runs three retrieval lanes in
parallel:

1. `knn_hunks` — vector KNN over **contextual embeddings** (good)
2. `bm25_hunks_by_text` — BM25 over **raw `diff_text`** (this is the gap)
3. `bm25_hunks_by_historical_symbol` (with HEAD-symbol fallback) — BM25
   over symbol names

The text-BM25 lane never benefits from the contextual preamble already
written into `hunk.semantic_text`. The lane that's most likely to match
*natural-language queries* (e.g. "fix retry on transient failures") is the
one denied the natural-language context.

A test-fake comment at `retriever.rs:491` records the intent and the
unfinished work:

> *"Plan 11: keep retriever tests focused on the existing three lanes
> until Task 4.1 wires the semantic lane in."*

Plan 11 Task 4.1 ended up addressing the symbol lane wiring instead;
the semantic-text lane wiring was deferred and not picked up since.

## Decision

Add `bm25_hunks_by_semantic_text` as a **4th retrieval lane**, fused via
the existing RRF + cross-encoder rerank pipeline. Do **not** replace the
raw-`diff_text` lane: the two lanes have complementary recall properties.

| Lane | Strength | Weakness |
|---|---|---|
| `bm25_hunks_by_text` (raw diff) | Matches inside deletions and context lines; preserves verbatim variable names | Noisy; no commit/file context |
| `bm25_hunks_by_semantic_text` (contextual) | Matches natural-language queries against commit-message vocabulary; dedicated symbol-name signal already in the preamble | Loses deletions and context lines; misses inline-comment text on those lines |

RRF with `k=60` (Cormack 2009) handles overlap gracefully — a hunk that
appears in both lanes gets a higher fused score, which is the desired
outcome.

## Profile gating

Add `semantic_text_lane_enabled: bool` to `RetrievalProfile`, defaulted
to `true` for every existing profile. Lane disablement is now governed
by the (already-existing) `profile.text_lane_enabled` flag for raw-diff
text and the new `profile.semantic_text_lane_enabled` flag for
contextual text. Profiles that want a specifically-contextual retrieval
(e.g. `bug_fix`, where commit-message vocabulary is high-signal) can in
a future change disable the raw-text lane in favour of the semantic-text
one; that's out of scope for plan-25 and follows once the eval data is
in.

## Scoring constants

No change. The existing `RankingWeights::lane_top_k = 100` applies to
the new lane; the existing `rerank_top_k = 50` and `recency_weight =
0.05` continue to gate the post-RRF pool. Per Anthropic's pool-size
discussion (and plan-23), widening `rerank_top_k` is a separate axis and
should be benchmarked independently of this change so the lane-wiring
delta is isolated in the eval data.

## Migration / compatibility

Zero migration burden:

- The `fts_hunk_semantic` table is V4 (already deployed).
- The V4 backfill seeded `semantic_text = diff_text` for pre-V4 hunks,
  so the new lane returns sensible (if context-free) results even on
  unreindexed corpora — same behavior as the existing raw-text lane.
- For corpora indexed under V4+, `semantic_text` already contains the
  full contextual preamble produced by `hunk_text::build`, so the new
  lane immediately benefits.
- No new storage trait methods, no SQL migration, no embedding model
  change.

## Eval acceptance criteria

The change ships only if the plan-10 context-engine eval shows:

1. `recall_at_5` stays at `1.0` (no regression).
2. `mrr` is `>=` the pre-change baseline (no rank degradation among the
   top-5).
3. At least one golden case demonstrably improves on the new lane —
   i.e. the per-failed-case dump shows the contextual preamble matched
   commit-message vocabulary the raw-diff lane missed.

If (1) or (2) regresses, the new lane is gated off by default
(`semantic_text_lane_enabled: false` on every profile) and the perf data
goes into the design follow-up.

## Out of scope

- Replacing the raw-diff text lane (deferred to a follow-up that needs a
  bigger eval corpus to safely measure).
- LLM-generated preambles (cost / index-time complexity outweighs the
  marginal gain over the deterministic preamble for git-derived
  metadata).
- Tuning per-lane RRF weights (the unweighted RRF in `query.rs:67-100`
  treats every lane equally; weighting is a future optimisation).
- Bumping `rerank_top_k` — covered by plan-23.
