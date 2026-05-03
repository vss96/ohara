# ohara v0.7 — query understanding and explain enrichment plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development or superpowers:executing-plans to
> implement this plan task-by-task. Run plan 10's eval harness before and
> after changing retrieval profiles.

**Goal:** route user intent to the right retrieval behavior instead of sending
every `find_pattern` query through one fixed profile, and make
`explain_change` answer "why" with surrounding change context rather than
blame rows alone.

**Architecture:** add a lightweight query-understanding layer in `ohara-core`
that extracts filters and selects a retrieval profile, keep the first version
rule-based and transparent, and enrich `explain_change` with related commits
from indexed history when available. MCP/CLI inputs remain stable; new behavior
is additive and surfaced in response metadata.

**Tech Stack:** Rust 2021, existing retriever/storage traits, serde response
metadata, no LLM dependency in v0.7.

---

## Phase 1 — Query Intent Model

### Task 1.1 — Define query intent and retrieval profile types

**Files:**
- Create: `crates/ohara-core/src/query_understanding.rs`
- Modify: `crates/ohara-core/src/lib.rs`
- Modify: `crates/ohara-core/src/query.rs`

- [x] **Step 1: Add `QueryIntent`.** Variants:
  `ImplementationPattern`, `BugFixPrecedent`, `ApiUsage`, `Configuration`,
  and `Unknown`.
- [x] **Step 2: Add `RetrievalProfile`.** Fields: lane weights or lane enable
  flags, `rerank_top_k`, `recency_weight`, optional `language`, optional
  `symbol_terms`, optional `path_terms`, and `explanation`.
- [x] **Step 3: Add serialization tests.** Response metadata should expose
  profile name and explanation without exposing unstable internal weights.

### Task 1.2 — Implement deterministic query parsing

**Files:**
- Modify: `crates/ohara-core/src/query_understanding.rs`

- [x] **Step 1: Add unit tests for intent classification.** Examples:
  "add retry like before" -> `ImplementationPattern`; "how did we fix timeout
  before" -> `BugFixPrecedent`; "where did we configure coreml" ->
  `Configuration`; unrecognized text -> `Unknown`.
- [x] **Step 2: Extract explicit filters.** Detect language hints (`rust`,
  `python`, `java`, `kotlin`), path-ish tokens (`src/foo.rs`), quoted symbol
  names, and simple timeframe phrases when they map cleanly to `since_unix`.
- [x] **Step 3: Keep confidence visible.** Return `confidence:
  High|Medium|Low` and include the parser's matched rules in debug metadata.

---

## Phase 2 — Profile-Aware Retrieval

### Task 2.1 — Thread profiles through `Retriever`

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs`
- Modify: `crates/ohara-core/src/query.rs`

- [x] **Step 1: Add a test for default behavior.** A query with no recognized
  intent must produce the same lane set and default weights as today's
  retriever.
- [x] **Step 2: Apply extracted filters.** If `PatternQuery.language` is
  already set by the caller, it wins over parsed language. Otherwise use the
  parsed language hint.
- [x] **Step 3: Adjust profiles conservatively.** Initial profile behavior:
  bug-fix queries raise recency slightly, API-usage queries favor symbol lanes,
  configuration queries favor text/semantic-text lanes, unknown queries use
  current defaults.
- [x] **Step 4: Surface profile metadata.** Add `_meta.query_profile` in CLI
  and MCP responses if their current response envelopes support metadata; if
  not, add it only to CLI first and plan an MCP schema bump separately.

### Task 2.2 — Add profile eval cases

**Files:**
- Modify: `tests/perf/fixtures/context_engine_eval/golden.jsonl`
- Modify: `tests/perf/context_engine_eval.rs`

- [x] **Step 1: Add cases that require different profiles.** Include a
  configuration query, a bug-fix precedent query, and an API usage query.
- [x] **Step 2: Record profile expectations.** Golden rows may include
  `expected_profile`; failures should print actual profile and confidence.
- [x] **Step 3: Gate behavior by evals.** If a profile adjustment improves one
  case but regresses another, leave the profile disabled and document the
  failed tradeoff in the PR.

---

## Phase 3 — Explain-Change Enrichment

### Task 3.1 — Add related commit lookup

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`
- Modify: `crates/ohara-storage/src/tables/explain.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [x] **Step 1: Add storage method `get_neighboring_file_commits`.** Inputs:
  repo id, file path, anchor SHA, limit before, limit after. Output:
  commit metadata plus touched hunk count for that file.
- [x] **Step 2: Add storage tests.** Given five commits touching a file,
  anchored at the middle commit, return two earlier and two later commits in
  deterministic timestamp/SHA order.
- [x] **Step 3: Keep it file-scoped.** Do not do semantic relatedness in this
  task; it should be a cheap indexed lookup.

### Task 3.2 — Add enriched explain response metadata

**Files:**
- Modify: `crates/ohara-core/src/explain.rs`
- Modify: `crates/ohara-cli/src/commands/explain.rs`
- Modify: `crates/ohara-mcp/src/tools/explain_change.rs`
- Modify: `docs-book/src/tools/explain_change.md`

- [x] **Step 1: Extend `ExplainMeta`.** Add `related_commits:
  Vec<RelatedCommit>` and `enrichment_limitation: Option<String>`.
- [x] **Step 2: Attach neighbors for blame hits.** For each blame anchor, add
  nearby same-file commits when `include_related` is true. Default true for
  CLI, false or capped for MCP if response size is a concern.
- [x] **Step 3: Preserve exact provenance.** Blame hits remain
  `Provenance::Exact`; related commits must be labeled as contextual, not
  exact line ownership.
- [x] **Step 4: Add e2e coverage.** A file modified across multiple commits
  should return the blamed commit plus neighboring same-file commits in meta.

### Task 3.3 — Rename the user-facing story

**Files:**
- Modify: `docs-book/src/tools/explain_change.md`
- Modify: `README.md`

- [x] **Step 1: Clarify the contract.** `explain_change` answers "which
  commits introduced these lines" exactly, and "what nearby changes shaped
  this area" contextually.
- [x] **Step 2: Add examples.** Show one response with exact blame hits and
  related commits so clients do not treat contextual commits as proof.

---

## Phase 4 — Optional LLM Hook (Deferred)

Do not implement in v0.7. After rule-based profiles and evals exist, consider
an optional local/remote LLM query rewriter only if the eval harness identifies
queries that deterministic parsing cannot handle.

The deferred design must keep these constraints:
- no network call by default;
- original user query remains part of retrieval;
- rewritten query and parser rationale are exposed in debug metadata;
- plan 10 evals must improve before enabling it by default.

---

## Done When

- [x] Unknown-intent queries preserve today's default retrieval behavior.
- [x] At least three golden eval cases assert expected profiles.
- [x] `explain_change` distinguishes exact blame evidence from contextual
  related commits in type names, docs, and serialized output.
- [x] MCP response size stays bounded by `k` and related-commit caps.
