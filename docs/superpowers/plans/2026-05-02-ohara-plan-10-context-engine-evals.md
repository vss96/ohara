# ohara v0.7 — context-engine eval harness plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development or superpowers:executing-plans to
> implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for
> tracking. This plan should land before changing retrieval semantics in
> plans 11 and 12.

**Goal:** create a repeatable retrieval-quality harness so changes to
chunking, symbol attribution, query profiles, rerank, and recency can be
judged by evidence instead of manual spot checks.

**Architecture:** add small fixture repos with known historical changes, a
JSONL golden-query format, and an ignored eval runner under `tests/perf` that
indexes each fixture, calls the same core retriever path the CLI/MCP use, and
reports recall@K, MRR, nDCG-lite, and latency. Keep the first version local,
deterministic, and small enough to run manually on developer machines.

**Tech Stack:** Rust 2021, existing `ohara-cli`/`ohara-core` APIs, serde JSONL,
temporary git fixtures, ignored cargo tests under `tests/perf`.

---

## Phase 1 — Golden Query Contract

### Task 1.1 — Define the eval case format

**Files:**
- Create: `tests/perf/fixtures/context_engine_eval/golden.jsonl`
- Modify: `tests/perf/README.md`

- [x] **Step 1: Document the JSONL schema.** Each row is one query:
  `id`, `query`, optional `language`, optional `since_unix`, `expected_shas`,
  `expected_paths`, and `notes`. `expected_shas` is ordered by importance;
  recall tests treat any match as success, MRR uses the first expected hit.
- [x] **Step 2: Add a tiny first corpus.** Include at least five cases:
  retry/backoff, timeout handling, config loading, error wrapping, and
  language-specific symbol lookup.
- [x] **Step 3: Add docs in `tests/perf/README.md`.** Explain that golden
  cases are not comprehensive benchmarks; they are regression tripwires for
  known product-critical queries.

### Task 1.2 — Build deterministic fixture repos

**Files:**
- Create: `tests/perf/build_context_eval_fixture.sh`
- Create: `tests/perf/fixtures/context_engine_eval/README.md`

- [x] **Step 1: Write a fixture builder script.** The script creates a small
  git repo under `target/perf-fixtures/context-engine-eval`, commits known
  changes with stable author/timestamps, and prints the final HEAD.
- [x] **Step 2: Ensure stable SHAs.** Set `GIT_AUTHOR_DATE`,
  `GIT_COMMITTER_DATE`, author name/email, and commit messages explicitly.
- [x] **Step 3: Record the generated commit roles.** The fixture README maps
  semantic labels such as `retry_backoff_commit` to the expected commit
  message. The eval runner can resolve SHAs at runtime from commit messages
  so the JSONL does not depend on hard-coded hashes if the fixture script is
  edited intentionally.

---

## Phase 2 — Eval Runner

### Task 2.1 — Add an ignored retrieval-quality test

**Files:**
- Modify: `tests/perf/Cargo.toml`
- Create: `tests/perf/context_engine_eval.rs`

- [x] **Step 1: Add a `#[ignore]` test.** The test runs the fixture builder,
  indexes the fixture with `ohara_cli::commands::index`, then calls the
  retriever for every JSONL case.
- [x] **Step 2: Compute metrics.** Emit one JSON line per run with:
  `cases`, `recall_at_1`, `recall_at_5`, `mrr`, `ndcg_lite`, `p50_ms`,
  `p95_ms`, and a list of failed case ids.
- [x] **Step 3: Fail only on clear regressions.** Initial thresholds:
  `recall_at_5 == 1.0`, `mrr >= 0.80`, and no individual query over
  2 seconds on the tiny fixture. Latency is a smoke signal, not the main
  contract.

### Task 2.2 — Make failures easy to diagnose

**Files:**
- Modify: `tests/perf/context_engine_eval.rs`

- [x] **Step 1: Print top hits for failed cases.** Include commit SHA,
  message first line, path, score, and provenance.
- [x] **Step 2: Include lane/debug metadata when available.** If the retriever
  does not expose lane contributions yet, leave this as a structured
  `lane_debug: null` field rather than adding a new retriever API in this
  task.
- [x] **Step 3: Document the command.** Add the exact run command to the test
  failure message: `cargo test -p tests-perf -- --ignored context_engine_eval
  --nocapture`.

---

## Phase 3 — CI and Workflow

### Task 3.1 — Add optional workflow coverage

**Files:**
- Modify: `.github/workflows/perf.yml`

- [x] **Step 1: Add a manual workflow path.** Extend the existing perf
  workflow so `workflow_dispatch` can run `context_engine_eval`.
- [x] **Step 2: Keep it informational.** Do not gate release until the
  harness has at least one release of stability.
- [x] **Step 3: Surface the metrics.** Print the JSON summary as a GitHub
  Actions notice, matching the QuestDB perf workflow style.

### Task 3.2 — Add a contributor rule

**Files:**
- Modify: `CONTRIBUTING.md`

- [x] **Step 1: Add retrieval-quality guidance.** Any PR that changes
  retrieval ranking, chunking, hunk text, symbol attribution, or query
  parsing must run the context-engine eval and paste the JSON summary.
- [x] **Step 2: Mention acceptable exceptions.** Pure docs, release workflow,
  and unrelated CLI presentation changes do not need the eval.

---

## Done When

- [x] `cargo test --workspace` remains green.
- [x] `cargo test -p tests-perf -- --ignored context_engine_eval --nocapture`
  produces a metrics JSON line and passes thresholds.
- [x] `tests/perf/README.md` explains how to add a new golden query.
- [x] The eval output is good enough to decide whether plans 11 and 12 improve
  or regress retrieval quality.
