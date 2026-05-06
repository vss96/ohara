# ohara — chunk-level embed cache + `--embed-cache` mode flag (Spec B, plan-27)

**Status:** RFC — ready to plan.

**Companion to:**
- `2026-05-05-ohara-plan-and-ignore-design.md` (Spec A — landed in plan-26).
- Spec D (parallel commit pipeline) — separately scoped, depends on this.

## Goal

Make repeat embeddings of identical chunk content cost zero. Add a
content-addressable cache of `(content_hash, embed_model) → vector`
that the embed stage consults before calling the embedder. Expose
three modes via a CLI flag so the user can A/B-test embed cost vs
retrieval quality:

```
ohara index --embed-cache off|semantic|diff
```

`off` is the default and matches today's behavior.

## Why this matters

Embedding dominates the cold-pass cost of `ohara index` (rough
breakdown on M-series CPU: parse ~30 ms/commit, embed ~80 ms/commit,
storage writes ~3 ms/commit). On giant repos a non-trivial fraction
of chunk content recurs:

- **Reverts and cherry-picks.** Same diff text replayed on a different
  commit.
- **Mass renames / vendor refreshes.** Identical chunk bodies appear
  in many commits.
- **Branch merges.** Hunks materialise into the topo walk via
  multiple parents.

Today every recurrence pays full embed cost. A content-addressable
cache turns each unique chunk into one embed call instead of N. Spec A
(`.oharaignore`) drops noise paths up front; this spec makes the
remaining work cheaper.

The `--embed-cache` flag is structured for benchmarking, not
configuration: the user picks a mode, runs `tests/perf/embed_cache_sweep.rs`,
and decides whether to flip the default.

## Shape

### CLI flag

```
ohara index --embed-cache <MODE>     # MODE ∈ off|semantic|diff
```

- `off` (default): today's behavior; no cache lookups; embedder
  consumes `semantic_text` (commit message + diff text).
- `semantic`: cache lookups keyed by `sha256(semantic_text)`. Embedder
  input unchanged. Hit rate driven by exact `(message, diff)` repeats
  — cherry-picks, reverts-with-same-message, true duplicates.
- `diff`: cache lookups keyed by `sha256(diff_text)`. **Embedder
  input changes to `diff_text` only** — commit message no longer
  contributes to the vector lane. Hit rate is much higher (vendor
  bumps, mass renames). Trade-off: the vector lane no longer encodes
  commit-message intent. Commit-message text remains separately
  indexed via `fts_hunk_semantic` (BM25), so retrieval still has a
  message-side signal; the *vector lane* simply specialises in
  diff-similarity.

### Storage change

A new refinery migration `crates/ohara-storage/migrations/V5__chunk_embed_cache.sql`:

```sql
CREATE TABLE chunk_embed_cache (
  content_hash TEXT NOT NULL,
  embed_model  TEXT NOT NULL,
  diff_emb     BLOB NOT NULL,    -- 384-float vector, same vec_codec as vec_hunk
  PRIMARY KEY (content_hash, embed_model)
);
```

No index — the primary-key compound is already the lookup key.

The cache lives in the same SQLite DB as `vec_hunk`. Per-repo, no
cross-repo sharing. Bounded by `unique(content_hash, embed_model)`
across the indexed history; on Linux-class repos that bound is
millions of entries × ~1.5 KB/vector ≈ low GBs at worst.

No eviction in v1. If pruning becomes necessary it gets its own RFC.

### Storage trait

`ohara-core::storage::Storage` gains two methods:

```rust
async fn embed_cache_get_many(
    &self,
    hashes: &[ContentHash],
    embed_model: &str,
) -> Result<HashMap<ContentHash, Vec<f32>>>;

async fn embed_cache_put_many(
    &self,
    entries: &[(ContentHash, Vec<f32>)],
    embed_model: &str,
) -> Result<()>;
```

Both ship a default implementation (`get_many` → empty map; `put_many`
→ no-op) so test storages stay light. `SqliteStorage` provides real
batched implementations.

### Indexer change

A new `EmbedMode` enum in `ohara-core`:

```rust
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EmbedMode {
    Off,
    Semantic,
    Diff,
}
```

`Indexer` gains a `with_embed_mode(EmbedMode)` builder. The mode
threads through to `Coordinator` and on to `EmbedStage`.

`EmbedStage` learns two builder methods, mirroring its existing
`with_embed_batch`:

```rust
impl EmbedStage {
    pub fn with_embed_mode(mut self, mode: EmbedMode) -> Self { ... }
    pub fn with_cache(mut self, storage: Arc<dyn Storage>, model: String) -> Self { ... }
}
```

When both are set, `EmbedStage::run` performs the following flow per
batch (signature unchanged from today; the new state is held on the
stage):

1. **Compute the embedder input per hunk.** `Off`/`Semantic` →
   `semantic_text`; `Diff` → `diff_text`.
2. **Hash inputs** with `ContentHash::from_text`. (Skipped when
   `mode = Off`.)
3. **Batch-lookup the cache** via `embed_cache_get_many`. (Skipped
   when `mode = Off`.)
4. **Partition** hunks into hits (cached vector reused) and misses
   (need embedding).
5. **Embed misses** in the existing `embed_batch` chunks.
6. **Write misses back to the cache** with `embed_cache_put_many`.
   (Skipped when `mode = Off`.) Empty `entries` short-circuits to
   no-op.
7. **Assemble the final `Vec<EmbeddedHunk>`** in the original input
   order, mixing cached and freshly-embedded vectors.

The fake/test embedder used in `coordinator/tests.rs` continues to
work — when `mode = Off`, the cache is never consulted, and the
existing tests need no fixture changes. New tests cover the
`Semantic` and `Diff` paths explicitly.

### `ContentHash::from_text`

The existing `ContentHash` newtype (plan-21) is keyed by git blob OID
(40-char hex). For chunk content addressing, add a sibling
constructor:

```rust
impl ContentHash {
    /// Hash arbitrary text content (UTF-8) for chunk-cache keys.
    /// Distinct from `from_blob_oid` — that one is keyed by git's
    /// blob hash for file content; this one keys cache lookups by
    /// the bytes the embedder will consume.
    pub fn from_text(text: &str) -> Self {
        let digest = sha2::Sha256::digest(text.as_bytes());
        Self(hex::encode(digest))
    }
}
```

`sha2` and `hex` are already workspace deps.

### Index-metadata integration

The mode is part of the index's identity. A user who indexed with
`--embed-cache semantic` cannot then `--incremental --embed-cache diff`
without rebuilding — the existing `vec_hunk` rows were computed from a
different embedder input.

Reuse the plan-13 `RuntimeIndexMetadata` / `StoredIndexMetadata` /
`CompatibilityStatus` machinery:

- Add `embed_input_mode: String` to `RuntimeIndexMetadata` (values:
  `"semantic"` for `Off|Semantic`, `"diff"` for `Diff`). `Off` and
  `Semantic` produce semantically equivalent vectors (both embed
  `semantic_text`), so they share the same metadata value — switching
  between them is safe.
- `CompatibilityStatus::assess` flags an `embed_input_mode` mismatch as
  `NeedsRebuild { reason: "embed_input_mode mismatch" }`.
- `ohara index` and `ohara index --incremental` consult the assessment
  before running and abort with the existing "run: ohara index --rebuild"
  hint on mismatch.

### CLI surface

`crates/ohara-cli/src/commands/index.rs` — gains `--embed-cache`:

```
ohara index [PATH] --embed-cache <MODE>     # MODE ∈ off|semantic|diff (default: off)
```

`ohara status` learns one new line:

```
embed_cache: semantic (12,431 cached vectors / 14.2 MB)
```

When the cache mode is `off`, the line is omitted.

## Constraints

- **Single-mode-per-index.** Mixing modes in the same `vec_hunk`
  produces incoherent KNN results. Enforced via `index_metadata`
  + `--rebuild` requirement on mode change.
- **`Off` and `Semantic` are vector-equivalent.** Both embed
  `semantic_text`; the only difference is whether the cache is
  consulted. Switching between them does not require `--rebuild`.
- **`Diff` requires a rebuild from `Off`/`Semantic`** (and vice versa).
- **Cache is never partially populated.** A crashed-mid-batch run can
  leave the cache without a vector that vec_hunk does have, but never
  the other way around — cache puts happen *before* the vec_hunk row
  is committed (the transaction order is: embed → cache_put → vec_hunk
  insert). On resume, the cache lookup either hits (cache survived) or
  the embedder is called again (cache row missing); both are safe.
- **No `unwrap()` / `expect()` outside tests** (existing rule).
- **All SQL lives in `ohara-storage`** (existing rule).
- **No new `--rebuild` autotrigger.** Mode mismatch produces a clear
  error message; the user runs `--rebuild` explicitly.

## Non-goals

- **Cross-repo embed sharing.** A team-shared cache would let
  developers reuse each other's embeddings, but introduces locking and
  trust concerns (cache poisoning). Out of scope.
- **Cache eviction / pruning.** Bounded by unique-content-hash; revisit
  if real-world data shows it growing without bound.
- **Cache export / import.** Shipping the cache via git or a tarball.
- **Re-embedding when the embedder model changes.** Already handled
  implicitly by the composite cache key — a new `embed_model` value
  produces zero hits and re-embeds. (The plan-13 compatibility check
  forces a rebuild on model change anyway, which clears `vec_hunk`;
  the cache survives across `--rebuild` since it's keyed by model.)
- **Hash function negotiation.** SHA-256 is the only supported hash;
  not configurable.
- **Quality eval harness.** A reusable retrieval-quality benchmarking
  framework. The `tests/perf/embed_cache_sweep.rs` operator harness in
  this spec is enough to inform the default-flip decision; building a
  reusable eval surface is its own project.

## Success criteria

- A unit test on `ContentHash::from_text` covers determinism (same
  input → same hash, different inputs → different hashes), and that
  `from_text("")` is well-defined and distinct from
  `from_blob_oid(empty_tree)`.
- A unit test on `SqliteStorage::embed_cache_get_many` and `_put_many`
  covers single-entry round-trip, batched round-trip (≥ 32 entries),
  composite-key uniqueness (same `content_hash` with different
  `embed_model` are distinct rows), and missing-key returns absent.
- A unit test on `EmbedStage::run` covers all three modes:
  - `Off`: cache is never consulted; all hunks pass through embedder.
  - `Semantic`: second invocation with identical inputs hits cache;
    embedder receives zero inputs.
  - `Diff`: embedder input is `diff_text` (no commit message);
    cache key is `sha256(diff_text)`.
- An integration test in `crates/ohara-cli/tests/`: index a fixture
  twice with `--embed-cache=semantic`. The second run consults the
  cache (assert via a counting fake embedder) and emits zero embedder
  calls.
- A regression test: index with `--embed-cache=semantic`, then run
  `--incremental --embed-cache=diff` → must error with the existing
  `CompatibilityStatus::NeedsRebuild` plumbing and recommend
  `--rebuild`.
- An operator perf harness `tests/perf/embed_cache_sweep.rs` runs the
  same fixture three times (`off`, `semantic`, `diff`) and prints embed
  wall-time + total chunks embedded + cache size. Manual; not in CI.
- `ohara status` shows `embed_cache: <mode> (<count> cached / <bytes>)`
  when cache mode is not `off`.

## Out of scope (deferred to companion specs)

- **Spec D — Parallel commit pipeline.** Worker pool + in-order
  watermark serializer. Sequenced after this spec; the cache surface
  this spec adds is concurrency-safe (storage trait methods are async
  and SQLite-WAL handles single-writer + many-reader fine).
- **Cross-repo cache.** Out of scope as noted above.
- **Reusable retrieval-quality eval framework.** Out of scope as noted.
- **CLI flag for cache pruning** (`ohara cache vacuum`). Future RFC if
  needed.

## Related

- `crates/ohara-storage/migrations/V5__chunk_embed_cache.sql` — new
  migration.
- `crates/ohara-storage/src/tables/embed_cache.rs` — new module: SQL
  for `embed_cache_get_many` / `_put_many`.
- `crates/ohara-storage/src/storage_impl.rs` — wire the new trait
  methods into `SqliteStorage`.
- `crates/ohara-core/src/types.rs` — `ContentHash::from_text(&str)`.
- `crates/ohara-core/src/storage.rs` — extend the `Storage` trait with
  `embed_cache_get_many` + `_put_many` (with default impls).
- `crates/ohara-core/src/embed.rs` (or wherever `EmbedMode` belongs)
  — new `EmbedMode` enum, `pub use` from `lib.rs`.
- `crates/ohara-core/src/indexer/stages/embed.rs` — restructured `run`
  to take mode + cache; partition hits/misses; assemble final
  `Vec<EmbeddedHunk>` preserving input order.
- `crates/ohara-core/src/indexer.rs` — `Indexer::with_embed_mode`
  builder; thread to `Coordinator`; pass to `EmbedStage::run`.
- `crates/ohara-core/src/index_metadata.rs` — `embed_input_mode`
  component in `RuntimeIndexMetadata` + assessment.
- `crates/ohara-cli/src/commands/index.rs` — `--embed-cache` clap arg.
- `crates/ohara-cli/src/commands/status.rs` — render `embed_cache:`
  line when present.
- `tests/perf/embed_cache_sweep.rs` — operator-run benchmark.
- `docs-book/src/architecture/indexing.md` — short section on the
  cache and what each mode does.
- Prior art: `2026-05-04-ohara-plan-21-explain-hydrator-and-blame-cache.md`
  (introduced `ContentHash` newtype that this spec extends).
