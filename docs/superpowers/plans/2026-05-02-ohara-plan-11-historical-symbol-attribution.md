# ohara v0.7 — historical symbol attribution and semantic hunk text plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development or superpowers:executing-plans to
> implement this plan task-by-task. Run plan 10's eval harness before and
> after this change. Prefer landing plan 13 first so metadata can record the
> semantic-text and attribution version change.

**Goal:** make `find_pattern` retrieve history at the symbol/hunk level
instead of using HEAD-only symbols joined by file path, and improve the text
that embeddings and rerank see.

**Architecture:** add a historical hunk-to-symbol attribution table populated
during indexing, add a normalized `semantic_text` representation for each
hunk, and adjust retrieval lanes so symbol-name matches point to hunks that
actually touched the matching symbol when that data exists. Raw diffs remain
the display/provenance artifact; semantic text is a search artifact.

**Tech Stack:** Rust 2021, git2 diff hunks, tree-sitter symbol spans,
rusqlite/refinery migrations, sqlite-vec, FTS5, existing `Storage` trait.

---

## Phase 1 — Data Model

### Task 1.1 — Add schema for semantic text and symbol attribution

**Files:**
- Create: `crates/ohara-storage/migrations/V4__historical_symbol_attribution.sql`
- Modify: `crates/ohara-storage/src/migrations.rs`
- Modify: `docs-book/src/architecture/storage.md`

- [x] **Step 1: Add migration tests.** Verify the migration adds:
  `hunk.semantic_text TEXT`, `hunk_symbol(hunk_id, symbol_kind, symbol_name,
  qualified_name, attribution_kind)`, and `fts_hunk_semantic`.
- [x] **Step 2: Define attribution kind.** Use string values:
  `exact_span`, `hunk_header`, and `file_fallback`. `exact_span` means a
  changed line intersects a parsed symbol span; `hunk_header` means git's hunk
  header identified an enclosing symbol; `file_fallback` keeps old behavior
  explicit and lower-confidence.
- [x] **Step 3: Backfill old rows conservatively.** Existing indexed hunks get
  `semantic_text = diff_text` and no `hunk_symbol` rows. Users can run
  `ohara index --force-history-attribution` in a later task to rebuild richer
  attribution.

### Task 1.2 — Add core types and storage contract

**Files:**
- Modify: `crates/ohara-core/src/types.rs`
- Modify: `crates/ohara-core/src/storage.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [x] **Step 1: Add `HunkSymbol` and `AttributionKind`.** Keep them small:
  hunk commit/path identity lives on the hunk; symbol rows carry kind/name and
  attribution confidence only.
- [x] **Step 2: Extend `HunkRecord`.** Add `semantic_text: String` and
  `symbols: Vec<HunkSymbol>`.
- [x] **Step 3: Update test fakes.** All existing core test storage fakes must
  compile with empty symbol vectors and `semantic_text = diff_text` until
  later tasks add richer data.

---

## Phase 2 — Build Semantic Hunk Text

### Task 2.1 — Introduce a semantic-text builder

**Files:**
- Create: `crates/ohara-core/src/hunk_text.rs`
- Modify: `crates/ohara-core/src/lib.rs`
- Modify: `crates/ohara-core/src/indexer.rs`

- [x] **Step 1: Write unit tests for text shape.** Given commit message,
  file path, language, change kind, symbol names, and raw diff, the builder
  emits sections in this order: `commit`, `file`, `language`, `symbols`,
  `change`, `added_lines`.
- [x] **Step 2: Extract added lines only.** Strip `+++`, `---`, hunk headers,
  deletions, and unchanged context. Keep raw `diff_text` untouched for display.
- [x] **Step 3: Wire into the indexer.** Embeddings use `semantic_text`;
  persisted hunks store both raw diff and semantic text.
- [x] **Step 4: Preserve compatibility.** If semantic text construction
  yields an empty body, fall back to raw diff text and mark no special
  attribution.

### Task 2.2 — Query FTS over semantic text

**Files:**
- Modify: `crates/ohara-storage/src/tables/hunk.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-core/src/storage.rs`

- [x] **Step 1: Add `bm25_hunks_by_semantic_text`.** Same filters as
  `bm25_hunks_by_text`, but targets `fts_hunk_semantic`.
- [x] **Step 2: Keep old lane available.** Do not delete raw diff BM25 until
  plan 10 evals show semantic text is strictly better or a fused combination
  beats both.
- [x] **Step 3: Add paired storage tests.** One query should match added-line
  content better in semantic text; one query should still be reachable through
  raw diff BM25.

---

## Phase 3 — Historical Symbol Attribution

### Task 3.1 — Attribute hunks to symbols

**Files:**
- Modify: `crates/ohara-parse/src/lib.rs`
- Modify: `crates/ohara-core/src/indexer.rs`
- Modify: `crates/ohara-git/src/diff.rs`

- [x] **Step 1: Expose symbol spans for a file blob.** Add a parser entry
  point that accepts file path, language, and source text, returning symbols
  with byte/line span metadata.
- [x] **Step 2: Map hunk line ranges to symbols.** Prefer post-image added
  line ranges. If a range intersects a parsed symbol, attach
  `AttributionKind::ExactSpan`.
- [x] **Step 3: Use hunk headers as fallback.** When parsing is unavailable
  but git hunk headers include an enclosing function/class name, attach
  `AttributionKind::HunkHeader`.
- [x] **Step 4: Avoid broad file fallback in the first pass.** Store no symbol
  row rather than pretending file-level attribution is symbol-level. Retrieval
  can still fall back to the existing HEAD-symbol/file lane while migration
  rolls out.

### Task 3.2 — Persist and query hunk symbols

**Files:**
- Modify: `crates/ohara-storage/src/tables/hunk.rs`
- Create: `crates/ohara-storage/src/tables/hunk_symbol.rs`
- Modify: `crates/ohara-storage/src/tables/mod.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`

- [x] **Step 1: Persist `hunk_symbol` rows transactionally with hunks.**
  `put_hunks` must delete prior hunk-symbol rows for rewritten commits before
  inserting replacements, matching existing resume safety.
- [x] **Step 2: Add `bm25_hunks_by_historical_symbol`.** Query symbol names
  directly from `hunk_symbol` and return `HunkHit`s ordered by BM25.
- [x] **Step 3: Add storage regression tests.** A file with two symbols should
  return only the hunk touching the queried symbol, not every hunk in the file.

---

## Phase 4 — Retrieval Integration

### Task 4.1 — Replace file-level symbol lane when historical data exists

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs`
- Modify: `crates/ohara-cli/tests/e2e_find_pattern.rs`

- [x] **Step 1: Add an e2e fixture with same-file unrelated symbols.** Query
  for `retry_policy`; expected rank 1 is the hunk that touched that symbol,
  not a different hunk in the same file.
- [x] **Step 2: Gather historical symbol lane.** Use
  `bm25_hunks_by_historical_symbol` as the primary symbol lane.
- [x] **Step 3: Retain HEAD-symbol fallback.** If the historical lane returns
  no hits, fall back to `bm25_hunks_by_symbol_name` so older indexes still work.
- [x] **Step 4: Run plan 10 evals.** Keep the change only if recall@5 stays
  green and at least one symbol-sensitive case improves.

### Task 4.2 — Populate `related_head_symbols`

**Files:**
- Modify: `crates/ohara-core/src/query.rs`
- Modify: `crates/ohara-core/src/retriever.rs`

- [x] **Step 1: Add a storage method for hunk symbols by id.** Return
  historical symbols first, HEAD symbols second if no historical rows exist.
- [x] **Step 2: Fill `PatternHit.related_head_symbols`.** Rename the field in
  a later breaking release; for now use it to carry related symbol names rather
  than leaving it empty.
- [x] **Step 3: Add serialization tests.** Ensure MCP/CLI JSON includes the
  populated list when symbols exist.

---

## Done When

- [x] Plan 10 evals pass before and after the change.
- [x] Symbol-name queries no longer retrieve unrelated same-file hunks when
  exact historical attribution exists.
- [x] Raw diff display remains unchanged.
- [x] Old indexes remain queryable after migration, with richer behavior only
  after reindex/rebuild.
