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
| **v0.6.0** | Throughput prep | `--profile` PhaseTimings JSON, `--embed-provider` auto-detect, `--resources` policy, CoreML/CUDA feature wiring, resume-crash fix, pinned progress bar. |

The [Changelog](./changelog.md) has a per-tag breakdown.

## In flight

### v0.6.1 / v0.7 — Phase 2B (gated on baseline data)

v0.6.0 shipped the measurement infrastructure (`--profile`, weekly
perf workflow, QuestDB fixture) and the hardware-acceleration
opt-in (`--embed-provider`, `--resources`, CoreML / CUDA feature
flags). What hasn't shipped yet is the actual throughput surgery —
those changes are gated on the QuestDB baseline data so we know we're
optimizing the right phase.

Candidates from Plan 6 Phase 2B, in rough order of expected impact:

- **Hunk-text trimming.** `total_diff_bytes / total_added_lines`
  from `PhaseTimings` is currently north of useful; the embed phase
  is paying for boilerplate it doesn't benefit from.
- **Pipeline parallelism.** The walk → embed → write path is
  serialized today. A bounded channel between phases lets the embed
  GPU/CoreML keep going while SQLite drains the previous batch.
- **Recency-first / partial index.** The (B) success criterion from
  the v0.6 RFC: index the newest-N commits first and backfill older
  history in the background, with `_meta` exposing what window is
  covered.

**Success criteria** (one of, unchanged from the v0.6 RFC):

- **(A)** First-time index of a QuestDB-class repo finishes in under
  **15 minutes** on a typical M-series laptop.
- **(B)** Time-to-useful is under **3 minutes**, with `_meta` clearly
  exposing what window has been covered.

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
