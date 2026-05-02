# ohara v0.7 — index metadata and rebuild safety plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development or superpowers:executing-plans to
> implement this plan task-by-task. This plan should land before any change
> that alters embedding dimensions, chunker output, parser behavior, or hunk
> semantic text.

**Goal:** make index compatibility explicit so Ohara can detect when an index
was built with old model/chunker/parser settings and tell users whether they
can continue, need a cheap migration, or must rebuild.

**Architecture:** add an `index_metadata` table keyed by repo id and component,
record the versions of embedding model, vector dimension, reranker model,
chunker version, parser versions, semantic-text version, and schema version,
then expose compatibility checks through CLI status, MCP metadata, and index
startup. This is a safety layer; it should not change retrieval ranking.

**Tech Stack:** Rust 2021, refinery migrations, rusqlite, existing
`IndexStatus`/`ResponseMeta`, CLI/MCP response metadata.

---

## Phase 1 — Metadata Schema

### Task 1.1 — Add `index_metadata`

**Files:**
- Create: `crates/ohara-storage/migrations/V3__index_metadata.sql`
- Modify: `crates/ohara-storage/src/migrations.rs`
- Modify: `docs-book/src/architecture/storage.md`

- [x] **Step 1: Add migration tests.** Verify `index_metadata` exists with:
  `repo_id`, `component`, `version`, `value_json`, and `recorded_at`.
- [x] **Step 2: Define component keys.** Initial keys:
  `schema`, `embedding_model`, `embedding_dimension`, `reranker_model`,
  `chunker_version`, `semantic_text_version`, and one parser key per language
  (`parser_rust`, `parser_python`, `parser_java`, `parser_kotlin`).
- [x] **Step 3: Backfill existing indexes.** On migration, write only
  `schema = current`. Runtime startup fills missing runtime metadata with
  `unknown` status; do not guess values for old indexes.

### Task 1.2 — Add core compatibility model

**Files:**
- Create: `crates/ohara-core/src/index_metadata.rs`
- Modify: `crates/ohara-core/src/lib.rs`
- Modify: `crates/ohara-core/src/query.rs`

- [x] **Step 1: Define `RuntimeIndexMetadata`.** This describes what the
  current binary expects: embedding model/dim, chunker version, parser
  versions, semantic text version, and schema.
- [x] **Step 2: Define `CompatibilityStatus`.** Values:
  `Compatible`, `QueryCompatibleNeedsRefresh`, `NeedsRebuild`, and `Unknown`.
- [x] **Step 3: Add unit tests.** Dimension mismatch -> `NeedsRebuild`;
  chunker version mismatch -> `QueryCompatibleNeedsRefresh`; missing metadata
  -> `Unknown`; exact match -> `Compatible`.

---

## Phase 2 — Storage Contract

### Task 2.1 — Add metadata storage methods

**Files:**
- Modify: `crates/ohara-core/src/storage.rs`
- Create: `crates/ohara-storage/src/tables/index_metadata.rs`
- Modify: `crates/ohara-storage/src/tables/mod.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [x] **Step 1: Add `get_index_metadata`.** Returns all component rows for a
  repo as typed key/value data.
- [x] **Step 2: Add `put_index_metadata`.** Replaces rows for the components
  passed by the caller; do not delete unrelated future component keys.
- [x] **Step 3: Add storage tests.** Round-trip all initial component keys and
  verify replacement is scoped to the component key, not the whole repo.

### Task 2.2 — Record metadata during indexing

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`
- Modify: `crates/ohara-embed/src/fastembed.rs`
- Modify: `crates/ohara-parse/src/lib.rs`

- [x] **Step 1: Expose embedder metadata.** `EmbeddingProvider` should expose
  model id and dimension through a small method or companion trait. Keep the
  method synchronous so status checks do not initialize heavy models.
- [x] **Step 2: Expose parser/chunker metadata.** Hard-code version constants
  in parser/chunker modules and bump them only when output semantics change.
- [x] **Step 3: Write metadata at successful index end.** Record metadata after
  hunks and HEAD symbols are persisted, before the final report returns.
- [x] **Step 4: Test partial failure behavior.** If indexing fails before the
  final write, metadata should not claim the new version is complete.

---

## Phase 3 — Status and Guardrails

### Task 3.1 — Add compatibility to `ohara status`

**Files:**
- Modify: `crates/ohara-cli/src/commands/status.rs`
- Modify: `docs-book/src/cli/status.md`

- [x] **Step 1: Show compatibility state.** Example output:
  `compatibility: query-compatible, refresh recommended (chunker_version)`.
- [x] **Step 2: Include actionable command.** If compatible but refresh
  recommended, print `ohara index --force`. If rebuild needed, print
  `ohara index --rebuild`.
- [x] **Step 3: Add CLI tests.** Cover compatible, unknown, and needs-rebuild
  cases without requiring a real model download.

### Task 3.2 — Add MCP metadata warnings

**Files:**
- Modify: `crates/ohara-mcp/src/tools/find_pattern.rs`
- Modify: `crates/ohara-mcp/src/tools/explain_change.rs`
- Modify: `docs-book/src/mcp-clients.md`

- [x] **Step 1: Add warning in `_meta`.** Queries should still run when status
  is `Unknown` or `QueryCompatibleNeedsRefresh`, but responses include a hint.
- [x] **Step 2: Fail early on `NeedsRebuild`.** If vector dimension or model
  incompatibility would produce wrong/failed KNN, return a structured error
  with the rebuild command rather than attempting retrieval.
- [x] **Step 3: Keep `explain_change` available when possible.** If only
  embedding metadata is incompatible, blame-based `explain_change` can still
  run with a warning because it does not use vectors.

### Task 3.3 — Add explicit rebuild command

**Files:**
- Modify: `crates/ohara-cli/src/commands/index.rs`
- Modify: `docs-book/src/cli/index.md`

- [x] **Step 1: Add `--rebuild`.** This deletes/recreates the repo index after
  user confirmation or a `--yes` flag. It is stronger than `--force`, which
  only refreshes HEAD symbols today.
- [x] **Step 2: Protect data loss.** Refuse `--rebuild` unless the target path
  resolves to a known repo id and the index directory is under `$OHARA_HOME`.
- [x] **Step 3: Add e2e coverage.** Build an index, record row counts, run
  rebuild, assert rows are recreated and metadata is current.

---

## Phase 4 — Documentation and Release Notes

### Task 4.1 — Document index compatibility classes

**Files:**
- Modify: `docs-book/src/architecture/indexing.md`
- Modify: `README.md`

- [x] **Step 1: Explain `--force` vs `--rebuild`.** `--force` refreshes
  derived symbol/chunker outputs; `--rebuild` recreates history and vectors.
- [x] **Step 2: Explain user-facing statuses.** Compatible means no action;
  query-compatible means results work but may miss quality improvements;
  needs-rebuild means retrieval should not proceed.

---

## Done When

- [x] `ohara status` can explain whether an index matches the current binary.
- [x] MCP responses include useful compatibility hints.
- [x] Vector/model mismatches do not produce silent bad search.
- [x] Future plans can bump version constants and rely on this safety layer
  instead of inventing one-off migration warnings.
