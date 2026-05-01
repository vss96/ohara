# ohara v0.3 — Retrieval pipeline (RRF + cross-encoder + AST chunker)

> **For agentic workers:** Use superpowers:subagent-driven-development or superpowers:executing-plans to implement task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> This plan is split into **four parallel tracks (A, B, C, D)**. A/B/C have non-overlapping file ownership and can be built concurrently by separate workers. D is the integration phase and runs after A, B, C all land.

**Goal:** Replace the v0.1–v0.2 hand-tuned linear ranker (`0.7·sim + 0.2·recency + 0.1·msg_sim`) with the *gather → fuse → rerank* pipeline from the v0.3 spec. Adds (1) FTS5 BM25 hunk-text and symbol-name lanes alongside the existing dense vector lane, (2) a cross-encoder rerank step using `bge-reranker-base` via `fastembed::TextRerank`, and (3) AST-aware sibling-merge chunking up to a 500-token budget. Recency survives only as a tiny multiplicative tie-breaker on the rerank score.

**Architecture:** `Retriever::find_pattern` becomes a three-lane gather (vec KNN | FTS5 hunk text | FTS5 symbol name) executed in parallel via `tokio::join!`, fused with Reciprocal Rank Fusion (k=60), then reranked through a `RerankProvider` trait (skipped if `None` or when `--no-rerank` is set). Storage migration V2 adds two FTS5 tables and a `sibling_names` column on `symbol`; the migration is cheap (FTS-backfill only, no re-embed). The chunking change is gated behind `ohara index --force` because it requires re-walking tree-sitter trees and re-embedding.

**Tech Stack:** Rust 2021, tokio, rusqlite 0.31 (FTS5 is bundled), fastembed `~4.9` (`TextRerank` + `RerankerModel::BGERerankerBase`), tree-sitter 0.22, refinery for SQL migrations, async-trait, anyhow, thiserror, tracing.

---

## 0. Findings that shape the design (verified during plan writing)

- **`fastembed::TextRerank` works with our pin.** `fastembed` 4.9.x exposes `TextRerank`, `RerankerModel::BGERerankerBase`, `RerankInitOptions`, and `RerankResult { document: Option<String>, score: f32, index: usize }`. The API is **synchronous** (no Tokio dep inside fastembed), so the impl follows the existing `FastEmbedProvider` pattern: hold the model in `Arc<Mutex<TextRerank>>` and run `rerank(...)` inside `tokio::task::spawn_blocking`. **No `ort` adapter is needed for v0.3.** Source: docs.rs/fastembed/4.9.1/fastembed/struct.TextRerank.html.
- **FTS5 BM25 ranking works in our SQLite build.** rusqlite 0.31 with `bundled` already ships FTS5; the V1 migration creates `fts_commit` and `fts_symbol`, both of which use the FTS5 vtable. The `bm25(<table>)` ranking function is a built-in of FTS5 (lower = more relevant; we negate or use `ORDER BY bm25(<t>) ASC`). No new dependency, no new pragma.
- **V1 schema baseline.** `crates/ohara-storage/migrations/V1__initial.sql` already has `symbol(id, file_path_id, kind, name, qualified_name, span_start, span_end, blob_sha, source_text)` and `fts_symbol(symbol_id UNINDEXED, qualified_name, source_text)`. V2 must add (a) `sibling_names TEXT NOT NULL DEFAULT '[]'` to `symbol`, (b) a *new* `fts_hunk_text(hunk_id UNINDEXED, content)` table, (c) a *new* `fts_symbol_name(symbol_id UNINDEXED, kind, name, sibling_names)` table that supersedes the lookup-purpose of the existing `fts_symbol`, and (d) backfill from existing rows. We keep `fts_symbol` for now (it's harmless and future tools may use it) — V2 is purely additive on the symbol side.
- **Existing `Storage` trait surface.** `Storage` lives in `crates/ohara-core/src/storage.rs`, not in the storage crate. Track A's trait additions edit that file plus `crates/ohara-core/src/lib.rs` (re-exports). Cross-track contract: D depends on A's trait shape.
- **Existing Retriever flow.** `Retriever::find_pattern` already pulls `q_emb`, calls `storage.knn_hunks`, and runs `rank_hits`. The pipeline rewrite replaces the body wholesale; the public method signature is unchanged so MCP/CLI callers don't notice.
- **`Symbol::sibling_names` is core-typed.** Adding a field to `ohara_core::types::Symbol` is a Track C change. Track A reads sibling_names through the storage layer (writing it to `fts_symbol_name`), and Track D never touches sibling_names. The Symbol struct change is the *only* cross-cutting type change; lock it down in §1.

---

## 1. Interface contracts (locked before any track starts)

These are the inter-track contracts. If any track diverges from these, integration breaks. Treat as load-bearing.

### 1.1 `Storage` trait additions (Track A owns; lives in `crates/ohara-core/src/storage.rs`)

```rust
#[async_trait]
pub trait Storage: Send + Sync {
    // ... existing methods unchanged ...

    /// BM25-ranked hunks whose `diff_text` matches the query (FTS5).
    /// Ordered best-first. `k` is the requested top-N before fusion.
    async fn bm25_hunks_by_text(
        &self,
        repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;

    /// BM25-ranked hunks whose touched files contain a symbol whose
    /// name (or sibling-merged name) matches the query.
    /// Ordered best-first.
    async fn bm25_hunks_by_symbol_name(
        &self,
        repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;
}
```

Both methods return the *same* `HunkHit` struct used by `knn_hunks`. The `similarity` field carries the BM25 score normalized to `1.0 / (1.0 + (-bm25_raw))` so callers can treat it as a positive "higher is better" number — but Track D does not consume the score, only the rank order, so the exact normalization is informational.

### 1.2 `RerankProvider` trait (Track B owns; lives in `crates/ohara-core/src/embed.rs`)

```rust
#[async_trait]
pub trait RerankProvider: Send + Sync {
    /// Score `candidates` against `query`. Output length == candidates length;
    /// element `i` is the score for `candidates[i]`. Higher is better.
    /// Implementations must be order-preserving with respect to the input.
    async fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<f32>>;
}
```

Track B's `FastEmbedReranker` impl wraps `fastembed::TextRerank::rerank(query, docs, return_documents=false, batch_size=None)` and returns `result.score` ordered by the *input* index (i.e. it reorders the fastembed `Vec<RerankResult>` by `result.index` so the output aligns with the caller's `candidates` slice).

### 1.3 `reciprocal_rank_fusion` (Track D owns; lives in `crates/ohara-core/src/query.rs`)

```rust
/// Hunk-id type used as the join key across the three retrieval lanes.
pub type HunkId = i64;

/// Reciprocal Rank Fusion. Each ranking is best-first.
/// Score for hunk h = sum over lanes of `1.0 / (k + rank_in_lane(h))`.
/// Hunks absent from a lane contribute 0 from that lane.
/// Returns hunk ids ordered best-first; ties broken by first-appearance.
pub fn reciprocal_rank_fusion(rankings: &[Vec<HunkId>], k: u32) -> Vec<HunkId>;
```

Default `k = 60` (Cormack et al.). The function is a free function and is `#[cfg(test)]`-tested in isolation; no I/O.

### 1.4 `RankingWeights` shape (Track D owns; lives in `crates/ohara-core/src/retriever.rs`)

```rust
pub struct RankingWeights {
    /// Recency multiplier on the rerank score. Default 0.05.
    pub recency_weight: f32,
    /// Recency half-life-ish constant (in days) for the exp-decay factor.
    /// Default 90.0 — a 90-day-old commit gets factor ≈ 0.37.
    pub recency_half_life_days: f32,
    /// Number of post-RRF candidates to feed into the cross-encoder.
    /// Default 50.
    pub rerank_top_k: usize,
    /// Per-lane gather size before fusion. Default 100.
    pub lane_top_k: u8,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            recency_weight: 0.05,
            recency_half_life_days: 90.0,
            rerank_top_k: 50,
            lane_top_k: 100,
        }
    }
}
```

The old `similarity` / `recency` / `message_match` fields are **deleted**. Any external caller that constructed `RankingWeights { .. }` with the old fields will fail to compile — intended; force a review.

### 1.5 `Symbol` struct change (Track C owns; lives in `crates/ohara-core/src/types.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file_path: String,
    pub language: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: Option<String>,
    /// Names of sibling AST nodes merged into the same chunk by the
    /// AST-aware sibling-merge chunker. Empty for v0.2-era rows or for
    /// chunks containing a single top-level symbol.
    pub sibling_names: Vec<String>, // NEW
    pub span_start: u32,
    pub span_end: u32,
    pub blob_sha: String,
    pub source_text: String,
}
```

Migration concern: Track A's V2 migration adds the SQL column `sibling_names TEXT NOT NULL DEFAULT '[]'`; Track A's storage layer serializes `Symbol::sibling_names` as JSON. Track C produces non-empty `sibling_names` only after `ohara index --force` re-runs the chunker (handled in Track D).

### 1.6 `Retriever::find_pattern` flag (Track D owns)

```rust
impl Retriever {
    pub fn with_reranker(self, r: Arc<dyn RerankProvider>) -> Self;
    pub fn with_no_rerank(self) -> Self;
    // existing find_pattern signature unchanged
}
```

Construction order: `Retriever::new(storage, embedder).with_weights(w).with_reranker(r)`. If no reranker is set, the pipeline returns the post-RRF top-K directly (degraded mode, not broken).

---

## File ownership map (non-overlapping in implementation phase)

| Track | Files (exclusive) |
|-------|-------------------|
| **A — Storage** | `crates/ohara-core/src/storage.rs` (trait additions only), `crates/ohara-core/src/lib.rs` (re-export `HunkId`), `crates/ohara-storage/migrations/V2__fts_text_and_symbol_name.sql` *(new)*, `crates/ohara-storage/src/hunk.rs`, `crates/ohara-storage/src/symbol.rs` *(new)*, `crates/ohara-storage/src/lib.rs`, `crates/ohara-storage/src/storage_impl.rs` |
| **B — Embed** | `crates/ohara-core/src/embed.rs` (trait addition only), `crates/ohara-embed/src/fastembed.rs` (separate impl block + new struct), `crates/ohara-embed/src/lib.rs`, `crates/ohara-embed/Cargo.toml` |
| **C — Parse** | `crates/ohara-core/src/types.rs` (`Symbol::sibling_names` field), `crates/ohara-parse/src/lib.rs`, `crates/ohara-parse/src/chunker.rs` *(new)*, `crates/ohara-parse/src/rust.rs`, `crates/ohara-parse/src/python.rs` |
| **D — Integration** | `crates/ohara-core/src/query.rs` (RRF), `crates/ohara-core/src/retriever.rs`, `crates/ohara-cli/src/commands/index.rs` (`--force`), `crates/ohara-mcp/src/tools/find_pattern.rs` (`--no-rerank`), `crates/ohara-cli/tests/e2e_find_pattern.rs`, `crates/ohara-cli/tests/e2e_rerank.rs` *(new)* |

`crates/ohara-core/src/storage.rs`, `embed.rs`, `types.rs` are each touched by exactly one track. `crates/ohara-core/src/lib.rs` is touched by A only (to re-export `HunkId`); B and C export through their own pub-mod statements. There is no shared file ownership in the implementation phase.

---

# Track A — Storage (FTS5 + Migration V2)

**Goal:** Land migration V2, populate FTS, expose `bm25_hunks_by_text` and `bm25_hunks_by_symbol_name` on `Storage`. Self-contained — no dependency on B or C.

**Owner files:**
- `crates/ohara-core/src/storage.rs`
- `crates/ohara-core/src/lib.rs` (single line: `pub use query::HunkId;` once D lands; A adds `pub type HunkId = i64;` directly in `storage.rs` until then)
- `crates/ohara-storage/migrations/V2__fts_text_and_symbol_name.sql` (new)
- `crates/ohara-storage/src/hunk.rs`
- `crates/ohara-storage/src/symbol.rs` (new)
- `crates/ohara-storage/src/lib.rs`
- `crates/ohara-storage/src/storage_impl.rs`

## A-1. Migration V2 SQL (red → green)

Add `crates/ohara-storage/migrations/V2__fts_text_and_symbol_name.sql`:

```sql
-- v0.3: add FTS5 BM25 lanes and a sibling_names column for AST-merged chunks.

ALTER TABLE symbol ADD COLUMN sibling_names TEXT NOT NULL DEFAULT '[]';

CREATE VIRTUAL TABLE fts_hunk_text USING fts5(hunk_id UNINDEXED, content);
CREATE VIRTUAL TABLE fts_symbol_name USING fts5(symbol_id UNINDEXED, kind, name, sibling_names);

-- Backfill from existing rows.
INSERT INTO fts_hunk_text (hunk_id, content)
  SELECT id, diff_text FROM hunk;

INSERT INTO fts_symbol_name (symbol_id, kind, name, sibling_names)
  SELECT id, kind, name, sibling_names FROM symbol;
```

- [ ] **A.1.r:** add `migrations_v2_creates_fts_tables_and_sibling_names_column` to `crates/ohara-storage/src/migrations.rs`. Assertions: `fts_hunk_text` exists, `fts_symbol_name` exists, `symbol.sibling_names` column exists with default `'[]'`. Run from in-memory db. Commit (red).
- [ ] **A.1.g:** add the V2 SQL file. `cargo test -p ohara-storage migrations_v2`. Commit (green).
- [ ] **A.1.r2:** add `migrations_v2_backfills_existing_hunks_and_symbols` — insert one hunk + one symbol via raw SQL under V1 schema, run V2, assert both `fts_hunk_text` and `fts_symbol_name` have one row each, with `sibling_names = '[]'`. Commit (red). *(Refinery applies migrations in order on a single connection, so this works by inserting between explicit V1 and V2 runs — see refinery `Runner::run_grouped` if needed; otherwise insert via raw SQL after a fresh in-memory db has only V1, then run the v2 migration alone via `Runner::set_target`.)*
- [ ] **A.1.g2:** verify the backfill clauses cover the assertion. Commit (green) only if a fix is needed; otherwise the test passes against A.1.g and the green is folded.

## A-2. `Storage` trait surface (red → green)

- [ ] **A.2.r:** edit `crates/ohara-core/src/storage.rs`. Add the two trait methods (signatures from §1.1). Add `pub type HunkId = i64;`. **Provide stub bodies that `unreachable!()` — do NOT add `#[async_trait]` defaults**, because every existing impl (e.g. `FakeStorage` in `query.rs` / `retriever.rs` tests) needs to be updated explicitly. Update the existing `PanicStorage` and `FakeStorage` test fixtures in `retriever.rs` and `query.rs` to add `unreachable!()` impls of both new methods. Run `cargo build -p ohara-core` — should pass. Run `cargo test -p ohara-core` — should pass. Commit. *(This is a pure interface commit; not formally a test commit, but it's the contract that B/C/D depend on, so land it first within the track.)*
- [ ] **A.2.r-tests:** add `bm25_hunks_by_text_orders_best_first` and `bm25_hunks_by_symbol_name_filters_by_language` to a new `crates/ohara-storage/src/storage_impl.rs` test module section (following the pattern of `knn_hunks_returns_nearest`). Insert three commits/hunks with deliberately distinguishable texts ("retry backoff", "renamed file", "timeout helper"); query for "retry"; assert the retry-text hunk is rank 0. Commit (red).
- [ ] **A.2.g:** create `crates/ohara-storage/src/symbol.rs` with `bm25_by_name(c, query, k, language, since_unix) -> Result<Vec<HunkHit>>`. The query joins `fts_symbol_name -> symbol -> file_path -> hunk -> commit_record`, groups by hunk id, picks min(bm25), orders by bm25 ASC. Add `bm25_by_text` to `crates/ohara-storage/src/hunk.rs` (joins `fts_hunk_text -> hunk -> file_path -> commit_record`). Wire both into `storage_impl.rs`. Make `crates/ohara-storage/src/lib.rs` re-export `pub mod symbol;`. Replace `unreachable!()` stubs in `SqliteStorage` with real impls. Commit (green).
- [ ] **A.2.r2:** add `bm25_hunks_by_text_respects_since_unix` and `bm25_hunks_by_text_returns_empty_for_no_match`. Commit (red).
- [ ] **A.2.g2:** any required impl tweaks. Commit (green).

### A scope, done-when

**Done when:**
- `cargo test -p ohara-storage --all-targets` passes.
- `cargo test -p ohara-core --all-targets` passes (the existing `FakeStorage`/`PanicStorage` need `unreachable!()` for the two new methods; D will replace those during integration).
- `cargo build --workspace` is green.
- The two BM25 methods return `HunkHit`s ordered best-first.

**Integration interface (what D will see):**
- `Storage::bm25_hunks_by_text` and `Storage::bm25_hunks_by_symbol_name` callable on `Arc<dyn Storage>`.
- `pub type HunkId = i64;` exported from `ohara_core::storage` (and re-exported through `lib.rs`).

**SQL note (best-first BM25):** SQLite's `bm25()` returns a **negative** number where larger-magnitude = more relevant; sort with `ORDER BY bm25(fts_hunk_text) ASC` (most-negative first). The `similarity` field stored in `HunkHit` should be `1.0 / (1.0 + (-bm25_raw))` to keep the "higher is better" convention; sort the returned `Vec` by that.

---

# Track B — Embed (RerankProvider trait + FastEmbedReranker)

**Goal:** Add the `RerankProvider` trait to core and a fastembed-backed impl. Self-contained — no dep on A, C.

**Owner files:**
- `crates/ohara-core/src/embed.rs`
- `crates/ohara-embed/src/fastembed.rs`
- `crates/ohara-embed/src/lib.rs`
- `crates/ohara-embed/Cargo.toml`

## B-1. `RerankProvider` trait

- [ ] **B.1.r:** add a unit test in `crates/ohara-core/src/embed.rs` (new `#[cfg(test)] mod tests`) that uses an in-tree `FakeReranker` returning `Ok(candidates.iter().map(|s| s.len() as f32).collect())` and asserts the output length and order alignment. This test exists primarily to document the contract. Commit (red).
- [ ] **B.1.g:** add the `RerankProvider` trait per §1.2 above. Commit (green).

## B-2. `FastEmbedReranker` impl

- [ ] **B.2.r:** add `crates/ohara-embed/src/fastembed.rs` test (gated `#[ignore]` like `embeds_returns_correct_dimension_and_count`) named `reranker_orders_relevant_doc_first`. Query `"how to retry on transient failures"`, candidates `["unrelated cooking recipe", "retry helper with exponential backoff", "delete user"]`. Assert `out[1] > out[0]` and `out[1] > out[2]` (i.e. the retry doc gets the highest score). Commit (red).
- [ ] **B.2.g:** implement `FastEmbedReranker` in `crates/ohara-embed/src/fastembed.rs`. Mirror `FastEmbedProvider` structure: `Arc<Mutex<TextRerank>>`, `tokio::task::spawn_blocking` wrapping `model.rerank(query, docs, /*return_documents=*/false, /*batch_size=*/None)`. After the call returns, **sort the `Vec<RerankResult>` by `r.index` ascending** so the output `Vec<f32>` of scores aligns positionally with the input `candidates` slice (fastembed sorts results by score by default). Re-export from `crates/ohara-embed/src/lib.rs`. No new entry in `Cargo.toml` — `fastembed` is already a workspace dep with `TextRerank` available in 4.9. Commit (green).

## B-3. Round-trip alignment test

- [ ] **B.3.r:** add `reranker_output_aligns_with_input_indices` (cheap test that doesn't load the model — uses the FakeReranker pattern but inside `crates/ohara-embed/src/fastembed.rs` we instead add a private helper `align_by_index(rs: Vec<RerankResult>, n: usize) -> Vec<f32>` and unit-test that helper directly). Commit (red).
- [ ] **B.3.g:** factor out and implement `align_by_index`. Commit (green).

### B scope, done-when

**Done when:**
- `cargo test -p ohara-core` passes (FakeReranker doc-test).
- `cargo test -p ohara-embed` passes (alignment helper test passes; reranker_orders_relevant_doc_first stays `#[ignore]` and is run manually with `--include-ignored` like the embedder test).
- `cargo build --workspace` is green.

**Integration interface (what D will see):**
- `ohara_core::embed::RerankProvider` trait.
- `ohara_embed::FastEmbedReranker` constructor `FastEmbedReranker::new() -> anyhow::Result<Self>`.

**Note on Cargo.toml:** the current `ohara-embed/Cargo.toml` already lists `fastembed.workspace = true`. No edit required unless you discover a feature flag is needed (verify by running `cargo check -p ohara-embed`).

---

# Track C — Parse (AST sibling-merge chunker)

**Goal:** Replace "one chunk per top-level symbol" with "AST-aware sibling merge up to 500 tokens." Add `Symbol::sibling_names`. Self-contained — no dep on A, B.

**Owner files:**
- `crates/ohara-core/src/types.rs`
- `crates/ohara-parse/src/lib.rs`
- `crates/ohara-parse/src/chunker.rs` (new)
- `crates/ohara-parse/src/rust.rs`
- `crates/ohara-parse/src/python.rs`

## C-1. `Symbol::sibling_names` field

- [ ] **C.1.r:** add a `serde` round-trip test in `crates/ohara-core/src/types.rs::tests` named `symbol_sibling_names_round_trip`. Construct a `Symbol` with `sibling_names: vec!["beta".into(), "gamma".into()]`, serialize, deserialize, assert equality. Commit (red).
- [ ] **C.1.g:** add the `sibling_names: Vec<String>` field per §1.5. Update both `crates/ohara-parse/src/rust.rs` and `crates/ohara-parse/src/python.rs` `extract` functions to set `sibling_names: vec![]` (preserves v0.2 behavior pre-chunker — single-symbol chunks). `cargo test -p ohara-core -p ohara-parse`. Commit (green).

## C-2. Chunker — algorithm and fixture test

The algorithm (from spec §"AST-aware sibling merging"):

1. Parse the file with tree-sitter (already done).
2. Collect the top-level symbol nodes the existing `extract()` already finds (Rust: `fn` / `impl` methods / `struct` / `enum`; Python: top-level `def` / `class` / methods). These are the *atoms*.
3. Walk atoms left-to-right (source-byte-order). Maintain a running chunk = `(primary: Symbol, siblings: Vec<Symbol>, tok_estimate: usize)` where `tok_estimate = chunk_source_len_chars / 4`.
4. For each next atom A:
   - If `current_chunk.tok_estimate + A.tok_estimate <= 500` → append A to `current_chunk.siblings`, extend the source span.
   - Else if `current_chunk` is non-empty → emit `current_chunk` as a `Symbol` (with `sibling_names = [siblings.iter().map(|s|s.name)]`, span = combined byte range, source_text = concat). Start a new chunk with A as primary.
   - If A alone exceeds 500 tokens → emit it on its own (don't subdivide).
5. Flush at end.

The emitted `Symbol`'s `name` is the *first atom*'s name (the "primary"). Its `kind` is the first atom's kind. `qualified_name` stays `None` for now. `source_text` is `&source[chunk.start..chunk.end]` (which may include whitespace between atoms — that's fine; it preserves locality).

### Fixture (from spec §"Testing strategy")

Three Rust functions of approximate token sizes 50, 600, 200, in source order. Expected:
- Chunk 1 = fn1 alone, tok=50 (next atom fn2 at 600 would push total to 650 > 500, so emit fn1 alone). *Wait — re-read*: spec says "chunk 1 = fn 1 + fn 3 merged at 250 tok; chunk 2 = fn 2 alone at 600 tok". That ordering only makes sense if **fn 2 is skipped over** because it alone exceeds 500 — i.e. the algorithm emits fn1 first, then fn2 alone, then continues merging from fn3. Re-spec'd:
  - Walk fn1 (50 tok). Try fn2 next: 50+600 > 500, so close current chunk (fn1 alone). Start new chunk with fn2 (600 tok > 500, so emit immediately as single-symbol chunk). Start new chunk with fn3 (200 tok). End-of-file → flush fn3.
  - Output: `[fn1 (50), fn2 (600), fn3 (200)]` — three chunks. **The spec's expected output is wrong**; see §"Spec defects" at end of plan. The plan implements the algorithm above; the test fixture asserts this corrected output. Cross-reference the spec patch.

- [ ] **C.2.r:** add `crates/ohara-parse/src/chunker.rs` with `pub fn chunk_symbols(atoms: Vec<Symbol>, max_tokens: usize, source: &str) -> Vec<Symbol>` skeleton returning `vec![]`. Add four tests in the same file:
  1. `chunker_emits_three_chunks_for_50_600_200_fixture` — assert names `["fn_a", "fn_b", "fn_c"]` (or whatever the fixture uses) and that chunk 0's `sibling_names == []` (single-atom because next would exceed budget).
  2. `chunker_merges_consecutive_small_atoms_into_one_chunk` — three 100-tok functions: assert exactly one chunk emitted with `sibling_names.len() == 2`.
  3. `chunker_emits_oversized_atom_alone` — one 800-tok function: assert one chunk, `sibling_names == []`.
  4. `chunker_preserves_source_byte_order_in_sibling_names` — assert `sibling_names` are in source order.

  Commit (red).
- [ ] **C.2.g:** implement `chunk_symbols` per the algorithm above. Token estimate `chars / 4`. Wire it into `crates/ohara-parse/src/lib.rs::extract_for_path` so callers go through `chunk_symbols(rust::extract(...)?, 500, source)` (and same for python). Commit (green).

## C-3. Plumb sibling_names through extraction

- [ ] **C.3.r:** add `extract_for_path_emits_sibling_names_for_merged_chunks` in `crates/ohara-parse/src/lib.rs::tests`. Use a tiny Rust source with 3 small fns; assert the returned `Vec<Symbol>` has length 1 and `sibling_names` populated. Commit (red).
- [ ] **C.3.g:** any wiring fix needed in `lib.rs::extract_for_path` to pass `source` through to `chunk_symbols`. Commit (green).

### C scope, done-when

**Done when:**
- `cargo test -p ohara-core -p ohara-parse` passes.
- `cargo build --workspace` is green.
- `extract_for_path("a.rs", source, sha)` returns AST-merged chunks with non-empty `sibling_names` when atoms merge.

**Integration interface (what D + A see):**
- `Symbol::sibling_names: Vec<String>` populated by parse.
- Track A's storage layer reads `Symbol::sibling_names` when persisting (during indexing) and writes the JSON to the `symbol.sibling_names` column. **A's V2 migration default `'[]'` covers the v0.2-era backfill path**; new indexed rows after C lands include real sibling names.

**Note on `put_head_symbols`:** today this is a no-op stub in `storage_impl.rs`. C does NOT change that; A is responsible for either (a) keeping the no-op and adding a follow-up TODO, or (b) implementing the persistence as part of A.2.g if convenient. *Recommended:* A keeps `put_head_symbols` as no-op for v0.3 — `find_pattern` reads symbols only via the `fts_symbol_name` table, which A backfills from the existing `symbol` rows + sibling_names column. The full re-build path is gated behind D's `--force` and is implemented in D-2 (which will call `Storage::put_head_symbols` with the new chunked symbols, requiring a real impl). **A action item:** implement `put_head_symbols` to insert into both `symbol` (with `sibling_names = serde_json::to_string(&s.sibling_names)?`) and `fts_symbol_name`. Add this to A-2.g.

---

# Track D — Core integration (sequential after A, B, C)

**Goal:** Replace the linear `rank_hits` with the gather → RRF → rerank pipeline. Add CLI/MCP flags. Land regression and rerank e2e tests.

**Owner files:**
- `crates/ohara-core/src/query.rs` (RRF helper + `HunkId` re-export)
- `crates/ohara-core/src/retriever.rs` (pipeline rewrite)
- `crates/ohara-cli/src/commands/index.rs` (`--force` flag)
- `crates/ohara-mcp/src/tools/find_pattern.rs` (`--no-rerank` input)
- `crates/ohara-cli/tests/e2e_find_pattern.rs` (regression)
- `crates/ohara-cli/tests/e2e_rerank.rs` (new)

## D-1. RRF (red → green)

- [ ] **D.1.r:** add `crates/ohara-core/src/query.rs::tests::rrf_*` cases:
  1. `rrf_combines_three_lanes_with_default_k` — three rankings: lane1 `[1,2,3]`, lane2 `[2,3,1]`, lane3 `[3,1,2]`. With k=60: each id appears in all three lanes; ranks are (lane1, lane2, lane3) for id 1 = (1,3,2); RRF score 1 = 1/61 + 1/63 + 1/62 ≈ 0.04826. Assert the *order* (id 1 wins because it is rank 1 in lane1; ties break by first-appearance for any equal-score case). The deterministic assert: result[0] is among `[1,2,3]`, all three present, length 3.
  2. `rrf_handles_disjoint_lanes` — lane1 `[10,20]`, lane2 `[30,40]`, lane3 `[]`. Length 4, all four ids present.
  3. `rrf_empty_input_returns_empty` — `rankings = []` returns `vec![]`.
  4. `rrf_canonical_paper_example` — pull a 3-system, 5-doc example from the Cormack 2009 paper appendix and assert the exact final order. *(If the paper isn't accessible quickly, replace with a hand-computed 2-lane example: lane1 `[a,b,c]`, lane2 `[c,a,b]` with k=60. Compute by hand: a = 1/61+1/62, b = 1/62+1/63, c = 1/63+1/61. a > c > b. Assert `["a","c","b"]`.)*

  Commit (red).
- [ ] **D.1.g:** implement `pub fn reciprocal_rank_fusion(rankings: &[Vec<HunkId>], k: u32) -> Vec<HunkId>` in `query.rs`. Use a `HashMap<HunkId, f64>` accumulator; after summing, collect into a `Vec<(HunkId, f64)>` and sort by score descending; on ties, preserve first-appearance order via a parallel `HashMap<HunkId, usize>` of first-seen index. Add `pub use crate::storage::HunkId;` to top of `query.rs` (or move the `pub type HunkId = i64;` definition here — pick one, and keep `storage.rs` re-exporting it). Commit (green).

## D-2. Pipeline rewrite (red → green)

- [ ] **D.2.r:** in `crates/ohara-core/src/retriever.rs::tests` (the existing test module), add `find_pattern_invokes_three_lanes_and_rrf`. Use a new `FakeStorage` (replacing the existing one in the same file) that records calls and returns hardcoded HunkHits per method. Use a `FakeReranker` that returns scores aligned with input order (e.g. `vec![3.0, 1.0, 2.0]`). Assert the output order matches the reranker's score order, not the RRF order. Commit (red).
- [ ] **D.2.g:** rewrite `Retriever::find_pattern`:
  ```rust
  pub async fn find_pattern(&self, repo_id, query, now_unix) -> Result<Vec<PatternHit>> {
      let q_text = vec![query.query.clone()];
      let q_emb_fut = self.embedder.embed_batch(&q_text);
      // tokio::join! the three lanes
      let (vec_hits, fts_hits, sym_hits) = tokio::join!(
          self.storage.knn_hunks(repo_id, ..., self.weights.lane_top_k, ...),
          self.storage.bm25_hunks_by_text(repo_id, &query.query, self.weights.lane_top_k, ...),
          self.storage.bm25_hunks_by_symbol_name(repo_id, &query.query, self.weights.lane_top_k, ...),
      );
      // fold each into Vec<HunkId> + a HashMap<HunkId, HunkHit> for later lookup
      let fused: Vec<HunkId> = reciprocal_rank_fusion(&[r1, r2, r3], 60);
      // truncate to rerank_top_k, hydrate to Vec<HunkHit>
      let candidates: Vec<&str> = hits.iter().map(|h| h.hunk.diff_text.as_str()).collect();
      let scores = match &self.reranker {
          Some(r) => r.rerank(&query.query, &candidates).await?,
          None => vec![1.0; candidates.len()], // degraded mode: keep RRF order
      };
      // apply recency tie-breaker
      let final = hits.into_iter().zip(scores).map(|(h, s)| {
          let age = ((now_unix - h.commit.ts).max(0) as f32) / 86400.0;
          let recency = (-age / self.weights.recency_half_life_days).exp();
          let score = s * (1.0 + self.weights.recency_weight * recency);
          PatternHit { ..., combined_score: score, similarity: h.similarity, recency_weight: recency, ... }
      }).collect();
      // sort by combined_score desc, take query.k
  }
  ```
  Delete the old `rank_hits` method and its tests. Note: the v0.2 "embed query → cosine vs commit messages" path is **gone**; FTS5 BM25 on hunk text replaces it. Commit (green).

  *Note on lane construction:* when `vec_hits` returns the same `HunkId` referenced by `fts_hits`, the lookup map (`HashMap<HunkId, HunkHit>`) keeps the first-seen `HunkHit`. The `HunkHit::similarity` field carries lane-specific scores but D doesn't surface them — they're informational; only RRF rank matters. To get `HunkId` out of a `HunkHit`, A must add a `pub hunk_id: HunkId` field to `HunkHit` (in `storage.rs`). **Add this to A-2.g.** *(This is a contract addendum; cross-referenced from §1.1.)*

- [ ] **D.2.r2:** add `find_pattern_no_rerank_returns_post_rrf_top_k` (Retriever built without a reranker). Assert ordering matches RRF directly (with recency multiplier still applied — but recency weight is 0.05, so the order should match RRF unless two recency factors differ wildly within the top-k, which they don't in the test fixture). Commit (red).
- [ ] **D.2.g2:** any branch wiring needed. Commit (green).

## D-3. CLI `--force` (red → green)

- [ ] **D.3.r:** add `crates/ohara-cli/tests/e2e_incremental.rs::index_force_rebuilds_chunked_symbols_and_reembeds`. Tempdir + tiny repo, run `ohara index` (gets v0.2-style symbols), then run `ohara index --force` and assert (a) the run reports a non-zero `head_symbols` count and (b) at least one `Symbol` row in the DB has non-empty `sibling_names` (read raw via `pool().get()` like other tests). Commit (red).
- [ ] **D.3.g:** add `#[arg(long)] pub force: bool` to `commands::index::Args`. When set: drop and re-create `symbol` and `vec_symbol` rows (don't blow away the whole DB), then run the indexer with the new chunker. Practically: add `Storage::clear_head_symbols(repo_id) -> Result<()>` to the trait — Track A owns the trait change but D adds it as part of integration (treat as a small A-side patch under D's coordination, and update both `unreachable!()` test fixtures). The CLI calls `clear_head_symbols` then proceeds with the normal indexer flow. Commit (green).

## D-4. MCP `--no-rerank` (red → green)

- [ ] **D.4.r:** add `crates/ohara-mcp/src/tools/find_pattern.rs::tests::no_rerank_field_parses_default_false` — assert serde default for the new flag is `false`. Commit (red).
- [ ] **D.4.g:** add `#[serde(default)] pub no_rerank: bool` to `FindPatternInput`. In `find_pattern` impl: if `input.no_rerank`, build a `Retriever` clone without a reranker (or call a new `Retriever::find_pattern_with(no_rerank=true)` shim). Cleanest: store the reranker as `Option<Arc<dyn RerankProvider>>` on `OharaServer` and pass-through; for one-call disable, pass a `&self` flag through to `find_pattern`. Add `Retriever::find_pattern_no_rerank` as a convenience wrapper that sets `self.reranker = None` for the duration of one call (use a clone of `self` with the reranker dropped — `Retriever` impls `Clone` on the `Arc` fields). Commit (green).

## D-5. e2e tests

- [ ] **D.5.r:** patch `crates/ohara-cli/tests/e2e_find_pattern.rs::find_pattern_returns_retry_commit_first` to (a) wire up a real `FastEmbedReranker` and (b) keep the assertion that the retry commit ranks #1. This is the **regression assert** — the v0.2 fixture must still return retry-first under the new pipeline. Commit (red — should still pass against the green from D.2.g; if not, the integration broke and we fix it). Treat this as a sanity commit; no pure-red expected.
- [ ] **D.5.r2:** add `crates/ohara-cli/tests/e2e_rerank.rs::cross_encoder_picks_better_message_among_near_duplicates`. Build a fixture with two commits whose hunks are byte-identical (same diff hash) but whose commit messages differ ("retry with backoff" vs "misc fixes"). Assert `hits[0].commit_message.contains("retry")`. Commit (red).
- [ ] **D.5.fixture:** extend `fixtures/build_tiny.sh` with a function `build_near_duplicates` invoked behind an env var `OHARA_FIXTURE_NEAR_DUPS=1`. The test sets that var before calling the fixture script. Commit (with the fixture extension; this is plumbing, not red/green).
- [ ] **D.5.g:** rerun. Both e2e tests pass under `cargo test --include-ignored`. Commit (green only if a fix was needed).

## D-6. Final pass

- [ ] **D.6:** `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --include-ignored`. Single commit if formatting/clippy fixes are needed.

### D scope, done-when

**Done when:**
- `cargo test --workspace` (without `--include-ignored`) passes.
- `cargo test --workspace -- --include-ignored` passes (this loads the embedder + reranker; ~120MB+110MB download on first run).
- `ohara index --force /path/to/repo` re-chunks and re-embeds.
- `find_pattern { ..., "no_rerank": true }` returns post-RRF results.

---

# Integration phase — landing order and conflicts

**Recommended landing order:**

1. **A first.** A adds the `Storage` trait methods, the V2 migration, and the `bm25_*` impls — and crucially adds `unreachable!()` stubs to both `FakeStorage` fixtures in `query.rs` and `retriever.rs` so the workspace stays green. This unblocks D.
2. **B and C in parallel.** B adds `RerankProvider` (no impact on storage/parse). C adds `Symbol::sibling_names` (impact: A's `put_head_symbols` impl becomes "for real"; the `'[]'` default in V2 SQL plus C populating an empty Vec by default keeps everything green pre-merge). Pure additive.
3. **D last.** D rewrites `Retriever::find_pattern`, deletes `rank_hits`, adds CLI/MCP flags, lands the e2e tests. D consumes A's `bm25_*`, B's `RerankProvider`, and C's chunker output.

**Expected merge conflicts:**

- `crates/ohara-core/src/lib.rs` — A re-exports `HunkId` from `storage`; D may also re-export from `query`. Resolve by keeping a single canonical re-export (prefer `storage::HunkId`; D adds `pub use storage::HunkId;` only if not already there). Likelihood: low.
- `crates/ohara-core/src/storage.rs` — A adds two trait methods + `HunkId` type + `HunkHit::hunk_id` field. D's coordination shim (`Storage::clear_head_symbols`) is also added here. Resolve by treating D-3.g as a small A-side patch landed during integration (adds one more `unreachable!()` to test fakes). Likelihood: medium — communicate.
- `crates/ohara-core/src/retriever.rs` — D rewrites this file end-to-end. The `unreachable!()` stubs A added become moot once D introduces a fresh `FakeStorage`. Resolve by D taking the file's content wholesale post-A.
- `crates/ohara-storage/src/storage_impl.rs` — A implements the new trait methods; D adds `clear_head_symbols`. Append-only; trivial to merge.
- `crates/ohara-parse/src/lib.rs` — C wires the chunker into `extract_for_path`; no other track touches this file.
- `crates/ohara-core/src/types.rs` — C adds `sibling_names`; no other track touches this file.

**Cross-cutting concern: `HunkHit` shape.** A must add `pub hunk_id: HunkId` to `HunkHit` (struct in `storage.rs`) so D's RRF pipeline can de-duplicate across lanes by hunk identity. This is *the* contract addendum that emerged during planning. A includes it in A-2.g; D depends on it.

**Cross-cutting concern: `Storage::put_head_symbols`.** Today it's a no-op stub. A implements it for real (insert into `symbol` + `fts_symbol_name`) so D-3.g (`--force`) can re-populate symbol-derived FTS rows after re-chunking. C produces `Symbol`s with sibling_names; A persists them; D triggers the pipeline.

---

# Testing strategy summary

| Layer | Track | Test name |
|-------|-------|-----------|
| migration | A | `migrations_v2_creates_fts_tables_and_sibling_names_column` |
| migration | A | `migrations_v2_backfills_existing_hunks_and_symbols` |
| storage unit | A | `bm25_hunks_by_text_orders_best_first` |
| storage unit | A | `bm25_hunks_by_symbol_name_filters_by_language` |
| storage unit | A | `bm25_hunks_by_text_respects_since_unix` |
| core unit | B | `RerankProvider` doc-test (FakeReranker) |
| embed unit | B | `align_by_index` |
| embed integ (`#[ignore]`) | B | `reranker_orders_relevant_doc_first` |
| core unit | C | `symbol_sibling_names_round_trip` |
| parse unit | C | `chunker_emits_three_chunks_for_50_600_200_fixture` |
| parse unit | C | `chunker_merges_consecutive_small_atoms_into_one_chunk` |
| parse unit | C | `chunker_emits_oversized_atom_alone` |
| parse unit | C | `chunker_preserves_source_byte_order_in_sibling_names` |
| parse unit | C | `extract_for_path_emits_sibling_names_for_merged_chunks` |
| core unit | D | `rrf_combines_three_lanes_with_default_k` |
| core unit | D | `rrf_handles_disjoint_lanes` |
| core unit | D | `rrf_empty_input_returns_empty` |
| core unit | D | `rrf_canonical_paper_example` |
| core unit | D | `find_pattern_invokes_three_lanes_and_rrf` |
| core unit | D | `find_pattern_no_rerank_returns_post_rrf_top_k` |
| cli e2e (`#[ignore]`) | D | `find_pattern_returns_retry_commit_first` (regression) |
| cli e2e (`#[ignore]`) | D | `cross_encoder_picks_better_message_among_near_duplicates` (new) |
| cli e2e | D | `index_force_rebuilds_chunked_symbols_and_reembeds` |
| mcp unit | D | `no_rerank_field_parses_default_false` |

**Informal benchmark (recorded in PR description, not committed code):**
- `time ohara query "retry with exponential backoff"` on `fixtures/tiny/repo`, before and after, with and without `--no-rerank`. Targets: P50 < 500 ms with rerank; P50 < 100 ms without.

---

# Standards (matching Plan 1 / Plan 2)

- TDD red/green commits per task: write failing test → commit (red) → write minimal implementation → commit (green). Refactor + commit only if needed.
- **No commit attribution.** No `Co-Authored-By` lines, no AI footer.
- Match the existing code style (rustfmt defaults, `cargo clippy --all-targets -- -D warnings` clean).
- Each track keeps the workspace green at every commit. When a red commit would otherwise fail to compile (e.g. adding a trait method), use the **stub-impl pattern**: add `unreachable!()` bodies to every existing impl/fake until the green commit replaces them.

---

# Files this plan touches (consolidated, by track)

```
[A] crates/ohara-core/src/storage.rs                                      [edit]
[A] crates/ohara-core/src/lib.rs                                          [edit, re-export]
[A] crates/ohara-storage/migrations/V2__fts_text_and_symbol_name.sql      [new]
[A] crates/ohara-storage/src/migrations.rs                                [edit, V2 test]
[A] crates/ohara-storage/src/hunk.rs                                      [edit, bm25_by_text]
[A] crates/ohara-storage/src/symbol.rs                                    [new, bm25_by_name]
[A] crates/ohara-storage/src/lib.rs                                       [edit, mod symbol]
[A] crates/ohara-storage/src/storage_impl.rs                              [edit, wire bm25 + put_head_symbols + clear_head_symbols]

[B] crates/ohara-core/src/embed.rs                                        [edit, RerankProvider]
[B] crates/ohara-embed/src/fastembed.rs                                   [edit, FastEmbedReranker]
[B] crates/ohara-embed/src/lib.rs                                         [edit, re-export]

[C] crates/ohara-core/src/types.rs                                        [edit, Symbol::sibling_names]
[C] crates/ohara-parse/src/lib.rs                                         [edit, wire chunker]
[C] crates/ohara-parse/src/chunker.rs                                     [new]
[C] crates/ohara-parse/src/rust.rs                                        [edit, sibling_names: vec![]]
[C] crates/ohara-parse/src/python.rs                                      [edit, sibling_names: vec![]]

[D] crates/ohara-core/src/query.rs                                        [edit, RRF + HunkId re-export]
[D] crates/ohara-core/src/retriever.rs                                    [rewrite]
[D] crates/ohara-cli/src/commands/index.rs                                [edit, --force]
[D] crates/ohara-mcp/src/tools/find_pattern.rs                            [edit, --no-rerank]
[D] crates/ohara-cli/tests/e2e_find_pattern.rs                            [edit, regression assert]
[D] crates/ohara-cli/tests/e2e_rerank.rs                                  [new]
[D] crates/ohara-cli/tests/e2e_incremental.rs                             [edit, --force test]
[D] fixtures/build_tiny.sh                                                [edit, near-duplicate fixture]
```

No `Cargo.toml` edits required: `fastembed.workspace = true` is already present in `ohara-embed`, and `TextRerank` ships with the same crate. If `cargo check -p ohara-embed` complains about a missing feature, add it then — but the docs.rs page for fastembed 4.9.1 lists `TextRerank` in the default-features public API, so no flag should be needed.

---

# Spec defects spotted (to patch before plan execution)

1. **Chunker fixture math is wrong.** Spec §"Testing strategy" says: "chunk 1 = fn 1 + fn 3 merged at 250 tok; chunk 2 = fn 2 alone at 600 tok." This implies the algorithm reorders or skips atoms, which the spec elsewhere explicitly forbids (depth-first source-order traversal). With source order `[50, 600, 200]` and a 500-token budget, the correct output is **three chunks** (`[50]`, `[600]`, `[200]`) — fn1 stops merging because adding fn2 (600) would overflow; fn2 emits alone because it exceeds budget; fn3 starts a fresh chunk and reaches end-of-file alone. **Patch:** spec §Testing should say "three chunks: fn 1 alone (50 tok), fn 2 alone (600 tok, oversized), fn 3 alone (200 tok)." Or, to actually exercise the merge path, change the fixture to source-order `[50, 200, 600]` and assert "chunk 1 = fn1+fn2 merged (250 tok); chunk 2 = fn3 alone (600 tok)." The plan implements the algorithm; the test in C-2.r matches the corrected fixture.

2. **Spec is silent on `HunkHit::hunk_id`.** RRF needs a stable join key across lanes, and today `HunkHit` does not expose the hunk's primary-key id (only `Hunk` content + `CommitMeta`). The plan adds `HunkHit::hunk_id: HunkId` (Track A). The spec should be amended to mention this in §Component changes / `ohara-storage`.

3. **Spec is silent on `Storage::clear_head_symbols`.** The `--force` re-index path needs to drop `symbol` and `vec_symbol` rows before re-populating. Today there is no such method; `put_head_symbols` is additive and a re-run would create duplicates. The plan adds `clear_head_symbols(repo_id) -> Result<()>` (Track A coord with D). Spec should mention this.

4. **Spec ambiguity on `RerankProvider` async-ness.** Spec §`ohara-embed` shows `async fn rerank(...)`. fastembed's `TextRerank::rerank` is synchronous. The plan keeps `async fn rerank` on the trait (matches the trait-friendly Send/Sync story) and wraps the sync call in `spawn_blocking` inside `FastEmbedReranker`, exactly like `FastEmbedProvider` does today. No spec change needed — this paragraph is a clarification.

5. **Spec under-specifies recency in degraded mode.** When `--no-rerank` is set, the pipeline returns post-RRF results. The recency multiplier still applies (rerank score = 1.0, recency factor still scales). This is intentional but not stated; the plan documents it in D-2.g and the e2e test asserts ordering accordingly.
