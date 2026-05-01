# Roadmap

## Released

| Version | Theme | Headline |
|---------|-------|----------|
| **v0.1** | Foundation | Plan 1 — workspace + SQLite/sqlite-vec/FTS5 schema + `find_pattern` MCP tool. |
| **v0.2** | Auto-freshness | Plan 2 — `ohara init` post-commit hook + `ohara index --incremental` fast path. |
| **v0.3** | Retrieval quality | Plan 3 — three-lane retrieval (vector + BM25 hunk-text + BM25 symbol-name) → RRF → cross-encoder rerank → recency tie-break. AST sibling-merge chunking. |
| **v0.4** | Java + Kotlin | Plan 4 — Java 17/21 (sealed types, records) and Kotlin 1.9/2.0 (data classes, objects). Annotations preserved in `source_text` for Spring-friendly retrieval. |
| **v0.5** | `explain_change` | Plan 5 — second MCP tool, deterministic git-blame-backed lookup of "why does THIS code look the way it does?". |
| **v0.5.1** | Polish | Progress bar, abort-resume hardening, `ohara update` self-update via axoupdater. |

The [Changelog](./changelog.md) has a per-tag breakdown.

## In flight

### v0.6 — indexing throughput

A first-time `ohara index` of a real-world Java/Kotlin repo (5k–50k
commits, hundreds of MB of pack data) currently takes hours. v0.6 is
about cutting that by an order of magnitude without regressing
retrieval quality.

**Phase 1 already shipped:** the `--profile` flag + `PhaseTimings`
report give per-phase wall-time and hunk-text inflation numbers
required to pick the right knob to turn. See
`docs/perf/v0.6-baseline.md` for the QuestDB baseline.

**Success criteria** (one of):

- **(A)** First-time index of a QuestDB-class repo finishes in under
  **15 minutes** on a typical M-series laptop.
- **(B)** Time-to-useful (newest-N-commits indexed first, older
  history backfilled in the background) is under **3 minutes**, with
  `_meta` clearly exposing what window has been covered.

(B) is interesting because it changes the user contract from "wait
for completion" to "useful immediately, quality improves." Reasonable
to ship both: (B) first, (A) eventually.

The full RFC is at
[`docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md`](https://github.com/vss96/ohara/blob/main/docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md).

## Considered for later

These come from the v0.3 spec's "Out of scope" list and the v0.6
RFC's deferred items. Not committed; revisited as evidence accrues.

- **LLM-distilled commit summaries** — replace `commit_msg +
  first_hunk` embedding with a "primary goal / key files / technical
  terms" summary against a local model. Wait until A/B against the
  v0.3 baseline shows it matters.
- **Bit-quantized vec index** — premature at our scale today;
  revisit once cold-index size matters more than retrieval latency.
- **Branch-reachability filter** — useful for users who switch
  branches often. Currently `find_pattern` returns hits across all
  reachable history.
- **Merkle file hashing** (Cursor-style) — only meaningful when ohara
  expands beyond commit history into current-state code retrieval.
- **HyDE query rewriting** — generate a hypothetical answer with a
  local LLM, embed *that*, and retrieve against the hypothetical.
  Mixed evidence in benchmarks; A/B before committing.
- **Query understanding pre-pass** — wait for evidence that
  raw-query embeddings underperform.
- **Task-specific retrieval profiles** — only meaningful once ohara
  has more than two MCP tools.
- **More languages** — Go, TypeScript, C# are the obvious next
  candidates. Mostly a tree-sitter grammar pin + a small
  node-kind → symbol-kind mapping.
- **Branch / PR-aware `explain_change`** — currently blame walks
  `HEAD`. Walking a feature branch's range against `main` is a
  natural extension.
