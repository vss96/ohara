# Retrieval pipeline

`find_pattern` is implemented as a four-stage pipeline: three parallel
retrieval lanes → Reciprocal Rank Fusion → cross-encoder rerank →
recency tie-break. This is the v0.3 architecture (Plan 3); the v0.1–v0.2
linear-blend formula was deleted in the same release.

## Architecture

```
find_pattern(query, k=5)
    │
    ├─► vector top-100         (sqlite-vec on vec_hunk, BGE-small)
    ├─► FTS5 hunk-text top-100 (BM25 on hunk body)
    └─► FTS5 symbol-name top-100 (BM25 on tree-sitter symbol names)
    │
    ▼
Reciprocal Rank Fusion (k = 60) → top-50 candidates
    │
    ▼
Cross-encoder rerank (bge-reranker-base, opt-out via --no-rerank)
    │
    ▼
Recency tie-break → top-K → response
```

## Stage 1: three retrieval lanes

Dispatched in parallel via `tokio::join!` from the `Retriever`:

- **Vector KNN.** Embed the query with BGE-small (384-dim), then run
  a `sqlite-vec` k-NN search against the `vec_hunk` table. Top-100.
- **BM25 hunk-text.** FTS5 BM25 over `fts_hunk_text(content)` —
  raw diff body. Top-100.
- **BM25 symbol-name.** FTS5 BM25 over
  `fts_symbol_name(kind, name, sibling_names)` — tree-sitter symbol
  identifiers from HEAD. Top-100.

Each lane returns a best-first list of `HunkId`s.

## Stage 2: Reciprocal Rank Fusion

Combine the three rankings with classic RRF (Cormack et al. 2009):

```
score(h) = Σ over lanes of  1 / (k_rrf + rank_in_lane(h))
where rank_in_lane is 1-based, k_rrf = 60
```

Hunks absent from a lane contribute 0 from that lane. Ties are broken
deterministically by first-appearance order across the input rankings.
Implemented in `ohara_core::query::reciprocal_rank_fusion` — a pure
function over `Vec<HunkId>` that's straightforward to unit-test.

The top 50 fused candidates feed the next stage.

## Stage 3: cross-encoder rerank

`bge-reranker-base` (~110 MB ONNX, CPU-only) scores each
`(query, hunk_text)` pair pointwise. The model downloads on first use
and is cached locally.

Opt-out: `find_pattern` accepts `no_rerank: true` (MCP) or
`--no-rerank` (CLI). When opted out, the `Retriever` skips the model
download entirely and returns the post-RRF order, with the recency
multiplier still applied. Useful when latency matters more than
top-1 precision.

## Stage 4: recency tie-break

A small multiplicative nudge applied after the cross-encoder score:

```
final_score = rerank_score * (1.0 + recency_weight * recency_factor)
where recency_factor = exp(-age_days / 90.0)   // 1.0 today → ~0.37 at 90 days
      recency_weight = 0.05                    // RankingWeights default
```

Recency does not feed into RRF or the cross-encoder score directly —
relevance dominates, recency only nudges within a tight relevance
band.

## Why this shape

All three of Augment, Cursor, and Cody converged on the same pattern:
**multi-stage retrieve → cross-encoder rerank**, **hybrid sparse +
dense retrieval before rerank**, and **AST-aware chunking that merges
siblings up to a token budget**. v0.3 ports those three choices into
ohara, scaled to a local-first single-binary deployment. The
[v0.3 retrieval design spec](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-01-ohara-v0.3-retrieval-design.md)
has the full research summary and the rationale for each parameter.

## Related

- [Storage schema](./storage.md) — what `vec_hunk`, `fts_hunk_text`,
  and `fts_symbol_name` look like on disk.
- [Language support](./languages.md) — what feeds `fts_symbol_name`.
- [`find_pattern` tool reference](../tools/find_pattern.md) — the
  user-visible entry point.
