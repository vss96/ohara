# Task 20: Plan 3 v0.3 retrieval pipeline — refactor backlog

Captured at HEAD `0ad33bb` (29 commits since `b46bd61`). `cargo clippy
--workspace --all-targets -- -D warnings` is clean; `cargo test
--workspace` reports 69 passed / 9 ignored / 0 failed (78 tests total;
plan claimed 80+).

This task encompasses Plan 3 in full: V2 migration with FTS5 BM25 lanes,
`Storage::bm25_hunks_by_text` + `bm25_hunks_by_symbol_name`, `HunkId`
join key, `HunkHit::hunk_id`, real `put_head_symbols`, `RerankProvider`
trait + `FastEmbedReranker`, `align_by_index` helper, `Symbol::sibling_names`,
AST sibling-merge chunker, `reciprocal_rank_fusion`, `Retriever::find_pattern`
rewritten as gather → RRF → optional rerank → recency, `RankingWeights`
v0.3 shape, CLI `--force` + `Storage::clear_head_symbols`, MCP `--no-rerank`
plumbing, `e2e_rerank` near-duplicate-commits test.

The HIGH-severity items at the top of this file are pre-release blockers
— they should be triaged before tagging v0.3.0. Medium/Low items are the
usual long-tail backlog.

---

## HIGH severity (release blockers)

### H1. Cross-encoder reranker is never wired into the production CLI/MCP code paths

- **Severity:** High
- **Location:** `crates/ohara-mcp/src/server.rs:27` (constructs `Retriever`
  with no `.with_reranker(...)`); `crates/ohara-cli/src/commands/query.rs:29`
  (same).
- **What:** The whole point of v0.3 is the cross-encoder rerank stage,
  but `OharaServer::open` and `commands::query::run` build the `Retriever`
  without ever attaching a `FastEmbedReranker`. Plan §D-4.g said "Cleanest:
  store the reranker as `Option<Arc<dyn RerankProvider>>` on `OharaServer`
  and pass-through" — that wasn't done. `with_reranker` is only called by
  the unit test in `retriever.rs:459` and the `e2e_rerank.rs` test fixture
  (line 98). Production users running `ohara query …` or invoking the
  MCP `find_pattern` tool always get the degraded post-RRF order, never
  the cross-encoder.
- **Why:** Ships v0.3 with the v0.3 quality story disabled at runtime.
  The MCP `--no-rerank` flag is also effectively a no-op because rerank
  is OFF by default. The `e2e_rerank` test passes because it manually
  attaches the reranker, masking the production gap.
- **Suggestion:** Add `reranker: Option<Arc<dyn RerankProvider>>` to
  `OharaServer`, build it in `open()` via
  `tokio::task::spawn_blocking(FastEmbedReranker::new)`. Pipe through
  `Retriever::with_reranker` at line 27. Mirror the change in the CLI
  `query` command. Decide on the lazy-load vs eager-load story (the
  reranker download is ~110 MB — same UX trade-off as the embedder; the
  existing pattern of "boot on first call, cache forever" applies). Add a
  smoke test that asserts the MCP server's retriever has a reranker
  attached (e.g., expose `Retriever::has_reranker() -> bool` for
  testability, or assert via behavior on a fixture where rerank-off and
  rerank-on diverge).
- **Effort:** S

### H2. `put_head_symbols` is INSERT-only with no dedup; every regular `ohara index` doubles the symbol table

- **Severity:** High
- **Location:** `crates/ohara-storage/src/symbol.rs:74-85` (`put_many`),
  `crates/ohara-core/src/indexer.rs:93-94` (`Indexer::run` always calls
  `put_head_symbols` after every pass)
- **What:** `Indexer::run` unconditionally extracts HEAD symbols and calls
  `storage.put_head_symbols` on every invocation. `put_many` does plain
  `INSERT INTO symbol` per atom — no upsert, no dedup, no check whether
  the table already has a HEAD snapshot. So:
  - First run: insert N rows. Total = N.
  - Second `ohara index` with one new commit: list_commits returns 1 → indexer
    runs the new commit *and* re-runs `put_head_symbols`, appending N more.
    Total = 2N.
  - Third run: 3N. Etc.

  The post-commit hook (`ohara index --incremental`, lines 48-61 of
  `commands/index.rs`) takes the early-return only when the watermark
  already points at HEAD. Right after a commit it never does (commit just
  moved HEAD), so every commit triggers a fresh `put_head_symbols` append.
  Over 100 commits the symbol table grows ~100×.

  The comment in `symbol.rs:70-73` even claims "v0.3 keeps the no-op
  semantics for repos that already have a populated `symbol` table" —
  but the code never enforces that.
- **Why:** Unbounded table growth, BM25 lane noise (duplicate symbol
  matches all collapse to one hunk_id via Rust-side dedup so query results
  stay "correct", but at growing CPU/IO cost), and storage-size bloat.
  The `--force` path is the only currently-correct flow because it calls
  `clear_head_symbols` first.
- **Suggestion:** Two viable fixes. Pick one:
  1. Make `put_head_symbols` itself idempotent: `clear_all` then
     `put_many` inside one transaction. This matches the comment's intent
     ("Replace HEAD-frame symbols").
  2. Push the clear into the indexer: have `Indexer::run` call
     `storage.clear_head_symbols(repo_id).await?` immediately before
     `put_head_symbols(repo_id, &symbols)`.

  Option 1 is more contained and matches the existing `clear_head_symbols`
  trait method. Add a regression test in `e2e_incremental.rs` that runs
  `ohara index` twice on the same repo (both `force: false`,
  `incremental: false`) and asserts `SELECT count(*) FROM symbol` doesn't
  double.
- **Effort:** S

### H3. `find_pattern_no_rerank_returns_post_rrf_top_k` does NOT exercise the `query.no_rerank: true` path

- **Severity:** High
- **Location:** `crates/ohara-core/src/retriever.rs:488-520` (the test
  itself), `crates/ohara-core/src/retriever.rs:173`
  (`match (&self.reranker, query.no_rerank)`)
- **What:** The test name suggests it covers `no_rerank=true`, but the
  `PatternQuery` it builds has `no_rerank: false` (line 510). The test
  passes because the `Retriever` is constructed without `.with_reranker(...)`,
  not because the flag is honored. `grep -rn "no_rerank: true"` across
  the entire repo returns no matches — nothing in the test tree exercises
  the `(Some(reranker), true)` short-circuit branch. The plan's required
  D-4.r MCP test `no_rerank_field_parses_default_false` was never
  implemented either.
- **Why:** A regression that broke the `no_rerank` short-circuit (e.g. a
  refactor that always called the reranker) would ship green. The MCP
  flag is also an exposed API surface on `FindPatternInput`, so the
  default-false serde behavior should be pinned by a unit test.
- **Suggestion:** (a) Rename the existing test to
  `find_pattern_with_no_reranker_returns_post_rrf_top_k` (it is
  characterizing degraded mode, which is fine and still useful). (b) Add
  a new test `find_pattern_no_rerank_flag_skips_attached_reranker`: build
  the retriever WITH a `ScriptedReranker` that records a call counter,
  set `q.no_rerank: true`, assert the reranker's call count stays at 0
  *and* the result order is RRF rather than reranker-driven. (c) Add the
  D-4.r-mandated `no_rerank_field_parses_default_false` test in
  `crates/ohara-mcp/src/tools/find_pattern.rs::tests` — `serde_json::from_str(
  "{\"query\":\"x\"}")` should yield `no_rerank: false`.
- **Effort:** XS

---

## Medium severity

### M1. Python class+method overlap corrupts merged-chunk `source_text`

- **Severity:** Medium
- **Location:** `crates/ohara-parse/src/python.rs:57-98` (extracts class
  and its methods as separate `Symbol`s with overlapping spans);
  `crates/ohara-parse/src/lib.rs:29` (`atoms.sort_by_key(|s| s.span_start)`);
  `crates/ohara-parse/src/chunker.rs:96-118` (`finish` slices
  `source[primary.span_start..self.span_end]`)
- **What:** The Python query produces two atoms per nested method: a
  `Class` symbol covering `[class_start, class_end)` and a `Method` symbol
  whose span `[method_start, method_end)` lies *inside* the class. After
  sort-by-span_start the chunker walks `[Class, Method]` in order and
  merges them when they fit the budget. `finish` then sets
  `span_end = Method.span_end` (which is mid-class) and slices
  `source[Class.span_start..Method.span_end]` — a truncated class body
  ending at the close of its first method. The corrupted `source_text`
  is persisted to `symbol.source_text`. Today nothing surfaces
  `source_text` to query callers (PatternHit doesn't include it), so the
  blast radius is "junk in the database column" rather than "wrong
  query results".
- **Why:** Plan §C-2 step 2 specifies "Collect the top-level symbol
  nodes the existing `extract()` already finds" and assumes they are
  source-disjoint atoms. The Python extractor pre-dates Plan 3 and
  emits nested overlapping atoms by design. Plan's "Spec defects"
  section caught the chunker fixture math bug but missed this.
- **Suggestion:** Pre-filter `atoms` in `extract_for_path` to drop atoms
  whose span is fully contained in a preceding (longer) atom — cheap
  O(n log n) sweep. Or change the Python query to capture only top-level
  atoms (functions and classes), letting the chunker treat the class as
  one opaque atom. Tree-sitter equivalent: anchor `function_definition`
  to module-level by adding a `parent: (module)` constraint, drop the
  method-inside-class capture entirely. Either way add a regression
  test that feeds Python source with a class containing methods and
  asserts the emitted chunk's `source_text` covers the entire class
  body, not a truncated prefix.
- **Effort:** S

### M2. Rust extractor double-emits methods; chunker treats them as siblings

- **Severity:** Medium
- **Location:** `crates/ohara-parse/queries/rust.scm:1-3` (pattern 1
  matches every `function_item`, pattern 2 also matches `function_item`
  inside `impl_item`); `crates/ohara-parse/src/rust.rs` (no dedup)
- **What:** Tree-sitter produces two matches for an `impl` method: once
  via `(function_item …) @def_function` (top-level pattern, gets emitted
  as `SymbolKind::Function`) and once via the `impl_item …` pattern
  (emitted as `SymbolKind::Method`). Both have identical
  `(span_start, span_end)`. `python.rs` has dedup-by-span; `rust.rs`
  doesn't. After sort-by-span_start the chunker sees them adjacent with
  identical spans, merges them, and inflates `running_tokens`,
  `sibling_names` (containing the duplicate method name), and the
  emitted chunk's apparent token cost.
- **Why:** Pre-existing pre-Plan 3 behavior, but Plan 3's chunker
  amplifies the impact. Symbol table grows with phantom duplicates and
  BM25 lane has 2× the rows for impl methods.
- **Suggestion:** Mirror python.rs's `HashMap<(span_start, span_end), Symbol>`
  dedup with kind-priority `Method > Function`. Or fix the Rust query
  so the top-level `function_item` pattern excludes methods inside an
  `impl_item` (negation lookahead in tree-sitter is awkward; the
  HashMap dedup is simpler).
- **Effort:** XS

### M3. BM25 lanes don't sanitize FTS5 query syntax — user input can crash the pipeline

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/hunk.rs:150` (`MATCH :query`);
  `crates/ohara-storage/src/symbol.rs:118` (same); MCP/CLI entry points
  in `crates/ohara-mcp/src/tools/find_pattern.rs:84-90` and
  `crates/ohara-cli/src/commands/query.rs:30-36`
- **What:** The user's natural-language string is bound directly into
  `MATCH :query`. FTS5 has an expression grammar (operators `AND`,
  `OR`, `NOT`, `NEAR`, prefix `*`, phrase `"…"`, column-filter `col:`).
  A query like `"async/await"`, `"foo*bar"`, or `"x AND"` becomes either
  a syntax error (rusqlite returns `Error::SqliteFailure(..., "fts5:
  syntax error near …")`) or unintended semantics. Empty strings raise
  `fts5: syntax error near """`. `?` propagation in the lane futures
  fails the whole pipeline (`fts_res?` in `retriever.rs:133`).
- **Why:** A casual user prompt with punctuation (`"how do we deal with
  re-tries"` — note the `-`) is a real failure mode. The vector lane
  doesn't care about syntax; the BM25 lanes do.
- **Suggestion:** Add a `fts5_quote(query: &str) -> String` helper in
  `ohara-storage` (or `ohara-core::query`) that wraps each whitespace-
  split token in `"…"` and escapes embedded `"` as `""`. Apply it at
  the call boundary in `bm25_by_text` and `bm25_by_name`. Also add an
  is-empty guard upstream in `Retriever::find_pattern` (skip both BM25
  lanes when query is empty rather than fail). Cover both via unit tests:
  one-token punctuation (`re-try`), multi-token (`async await`), empty,
  and FTS5-operator-as-literal (`AND`).
- **Effort:** S

### M4. `bm25_by_name` has no SQL-level `LIMIT`; on busy repos the join can return huge result sets before Rust-side truncation

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/symbol.rs:109-153`
- **What:** Unlike `hunk::bm25_by_text` (which has `LIMIT :k`),
  `symbol::bm25_by_name` orders by BM25 ascending and pulls *all*
  matching rows from the join `fts_symbol_name → symbol → file_path →
  hunk → commit_record`. Dedup-by-hunk-id and truncate-to-k happen in
  Rust afterward. The comment explains the dedup rationale, but the
  unbounded scan is unmitigated. A "hot" file with N historical hunks
  and M matching symbols produces N×M rows that all funnel into one
  hunk_id post-dedup. For a popular function name on a busy repo this
  can be tens of thousands of rows pulled across the C ABI before
  truncation.
- **Why:** Latency degrades nonlinearly with repo age. Plan's informal
  benchmark target ("P50 < 500 ms with rerank") doesn't reflect this.
- **Suggestion:** Add `LIMIT :limit` with `:limit = (k as i64) * 50` (or
  some safe multiplier — needs to cover the dedup-collapse worst case
  but not be unbounded). Alternatively, run a two-stage query: first
  CTE picks top-N symbol_ids by BM25, then join only those to hunks.
  Add a perf-flavored test with a fixture seeding 100+ hunks per file
  and assert the lane returns within a reasonable wall-clock target.
- **Effort:** S

### M5. PatternHit `recency_weight` field stores the recency *factor*, not the *weight* — misleading name

- **Severity:** Medium
- **Location:** `crates/ohara-core/src/query.rs:34` (`pub recency_weight: f32`),
  `crates/ohara-core/src/retriever.rs:185-202` (computes
  `recency = exp(-age/half_life)` then assigns
  `recency_weight: recency`)
- **What:** The serialized `PatternHit.recency_weight` is the per-hit
  decay factor (a number in `(0, 1]`), not the global weight constant
  (`RankingWeights::recency_weight = 0.05`). The two are different
  quantities; the JSON consumer (e.g. an MCP client trying to interpret
  the response) sees a field named `recency_weight` but reads back the
  factor. This is a serialized-API surface that ships in v0.3.0.
- **Why:** Field name was carried forward from v0.2 when its semantics
  changed; nobody renamed it. Once the v0.3 JSON shape ships and clients
  start consuming `_meta.hits[*].recency_weight`, renaming becomes a
  breaking change.
- **Suggestion:** Rename to `recency_factor` in both `PatternHit` and
  the assignment site. Pre-1.0 is the cheap window. Update the existing
  `pattern_hit_serializes_to_expected_json_shape` test
  (`query.rs:124-143`) to assert the new field name.
- **Effort:** XS

### M6. `--force` on a fresh (never-indexed) repo is untested

- **Severity:** Medium
- **Location:** `crates/ohara-cli/tests/e2e_incremental.rs:133-223`
  (`index_force_rebuilds_chunked_symbols_and_reembeds` runs
  `force: false` then `force: true` — never `force: true` first)
- **What:** The `--force` test seeds the index with a normal run before
  exercising the force path. The fresh-repo case (`ohara init` then
  `ohara index --force` immediately, with no prior index) takes the same
  code path but exercises `clear_head_symbols` against empty tables and
  `SqliteStorage::open` against a brand-new DB. Today this works
  because `DELETE FROM` on an empty table is a no-op, but a future
  refactor that (e.g.) added a "must have non-zero symbol_id" assertion
  would silently regress.
- **Why:** Fresh-repo + `--force` is a plausible workflow; advised by
  the migration log message in the spec ("re-run with --force for full
  v0.3 benefits"). Worth pinning.
- **Suggestion:** Add `index_force_on_fresh_repo_is_equivalent_to_full_index`
  to `e2e_incremental.rs`: tempdir + 2 commits, run only `force: true,
  incremental: false`, assert `report.new_commits == 2`,
  `report.head_symbols > 0`, and `SELECT count(*) FROM symbol` matches
  `report.head_symbols` (i.e., no duplicates).
- **Effort:** XS

### M7. V2 backfill path on truly-empty V1 (no hunks, no symbols) is implicit-only

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/migrations.rs:106-172`
  (`migrations_v2_backfills_existing_hunks_and_symbols`)
- **What:** The backfill test seeds one hunk and one symbol pre-V2 and
  asserts the `fts_*` mirror tables have one row each. The empty case
  (V1 applied, no inserts, V2 runs) is covered transitively by
  `migrations_apply_to_fresh_db` and
  `migrations_v2_creates_fts_tables_and_sibling_names_column`, neither
  of which seeds rows — so we know V2 *runs* against empty tables, but
  no test asserts "the backfill INSERT-SELECT pulled 0 rows and
  fts_hunk_text / fts_symbol_name are empty". A future migration tweak
  that, say, joined `hunk` to a non-existent table would break only on
  fresh databases and pass the existing tests.
- **Why:** The empty-V1 path is the actual fresh-install path on every
  new user's machine — worth pinning.
- **Suggestion:** Add `migrations_v2_backfill_is_a_noop_when_v1_tables_are_empty`:
  apply V1, do NOT seed, apply V2, assert `count(*)` is 0 on both
  `fts_hunk_text` and `fts_symbol_name`.
- **Effort:** XS

### M8. `clear_head_symbols` deletes from `vec_symbol`, but nothing ever populates `vec_symbol` — dead delete

- **Severity:** Medium
- **Location:** `crates/ohara-storage/src/symbol.rs:64`
  (`tx.execute("DELETE FROM vec_symbol", [])`)
- **What:** V1 schema (`migrations/V1__initial.sql:62`) creates
  `vec_symbol(symbol_id INTEGER PRIMARY KEY, source_emb FLOAT[384])`,
  but `symbol::put_one` (and the indexer it's called from) never
  inserts into `vec_symbol`. Symbol embeddings simply aren't computed
  in v0.3 (the BM25-by-name lane doesn't need them). So the
  `DELETE FROM vec_symbol` in `clear_all` always operates on an empty
  table.
- **Why:** Not a bug, but it's mis-leading code: the comment claims
  the schema-level cascade clears `vec_symbol` rows. There's no schema
  cascade; there's a plain DELETE on an always-empty table. Either
  populate `vec_symbol` (per V1's apparent intent) or document why it
  exists but is unused.
- **Suggestion:** Either (a) drop the `DELETE FROM vec_symbol` line
  and the comment; document at the V1-schema level that `vec_symbol`
  is reserved for a future symbol-embedding-based lane (deferred per
  v0.3 spec); (b) keep the DELETE as defensive and note it's a no-op
  today. Pick (a) — dead code is more confusing than missing safety.
- **Effort:** XS

### M9. Spec defect #1's chunker fixture rewrite never made it back into the spec

- **Severity:** Medium
- **Location:** `docs/superpowers/specs/2026-05-01-ohara-v0.3-retrieval-design.md:166-174`
  (still says "chunk 1 = fn 1 + fn 3 merged at 250 tok; chunk 2 = fn 2
  alone at 600 tok"); plan §"Spec defects spotted" item 1 calls this
  out and proposes a corrected fixture
- **What:** Plan 3 §"Spec defects" §1 caught the inconsistent fixture
  ("chunk 1 = fn 1 + fn 3 merged" implies reordering, which the spec
  forbids elsewhere) and the test in `chunker.rs:154-175` implements the
  corrected behavior (3 chunks for `[50, 600, 200]`). But the spec
  itself was never amended. Plan said "Patch: spec §Testing should say
  …" — that patch wasn't applied.
- **Why:** Plan-spec drift. A future reader landing on the spec without
  reading the plan will be confused.
- **Suggestion:** Apply the spec patch from plan §"Spec defects" §1.
  Either delete the buggy bullet or rewrite the fixture to one that
  actually exercises the merge path (`[50, 200, 600]` → "chunk 1 =
  fn1+fn2 merged (250 tok); chunk 2 = fn3 alone (600 tok)").
- **Effort:** XS

---

## Low severity

### L1. CLI `query` command has no `--no-rerank` flag

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/query.rs:8-20`
- **What:** `commands::query::Args` exposes `query`, `k`, `language` —
  no `no_rerank`, no `since`. The MCP tool exposes both. Once H1 wires
  the reranker into the CLI, opt-out parity matters: a user wanting the
  fast path should be able to set it. Not strictly Plan 3 scope (Plan
  3's user-facing surface was MCP-first), but worth flagging.
- **Why:** Behavior parity between MCP and CLI front-ends.
- **Suggestion:** Add `#[arg(long)] pub no_rerank: bool` and
  `#[arg(long)] pub since: Option<String>` to `Args`, plumb through
  to `PatternQuery`. Reuse `parse_since` from
  `crates/ohara-mcp/src/tools/find_pattern.rs` (or extract to
  `ohara-core::query`).
- **Effort:** XS

### L2. RankingWeights not exposed for tuning at the CLI/MCP layer

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:25-50`,
  `crates/ohara-mcp/src/tools/find_pattern.rs:38-56`
- **What:** `RankingWeights` (recency_weight, half_life_days,
  rerank_top_k, lane_top_k) is constructed once with `Default::default()`
  and never overridden by callers. Plan §1.4 made it a public struct
  expecting it to be tunable; today there's no CLI flag, no MCP input
  field, no env var. Hard-coded defaults are fine for v0.3.0 but worth
  flagging that the surface exists in core but is invisible at the
  user boundary.
- **Why:** Reviewers tuning retrieval quality across repos would have
  to recompile.
- **Suggestion:** No action for v0.3.0 (defaults are reasonable). When
  the first quality complaint lands, expose `recency_weight` via env
  var `OHARA_RECENCY_WEIGHT` or a `[ranking]` section in a future
  config file.
- **Effort:** S (when triggered)

### L3. RRF unit test `rrf_combines_three_lanes_with_default_k` is a weak smoke test

- **Severity:** Low
- **Location:** `crates/ohara-core/src/query.rs:249-262`
- **What:** Three permuted lanes `[1,2,3]`, `[2,3,1]`, `[3,1,2]`. The
  hand-computed scores: id 1 = 1/61 + 1/63 + 1/62; id 2 = 1/62 + 1/61 +
  1/63; id 3 = 1/63 + 1/62 + 1/61. All three sums are equal (commutative
  addition). So all three IDs end up tied; tie-break is first-appearance,
  which yields `[1, 2, 3]`. The test only asserts presence and length,
  not order — so it doesn't actually verify the score-by-rank logic;
  any function that returns a permutation of `{1,2,3}` would pass.
- **Why:** The other RRF tests (`rrf_two_lane_hand_computed_example`,
  `rrf_handles_disjoint_lanes`) cover the substantive behavior; this
  one is essentially a no-op assertion. Plan §D-1.r §1 already noted
  the deterministic-assert problem and proposed only "result[0] is
  among `[1,2,3]`, all three present, length 3" — which is what
  landed. Fine, but worth tightening.
- **Suggestion:** Either delete it (redundant with `rrf_two_lane_hand_computed_example`)
  or change the lane order to one that produces non-tied scores (e.g.
  `[1,2,3], [1,3,2], [1,2,3]` — id 1 scores three rank-1s, id 2 two
  rank-2s, id 3 a rank-3 + rank-2 + rank-3) and assert the exact
  order.
- **Effort:** XS

### L4. `align_by_index` silently drops out-of-range and missing indices instead of erroring

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:141-149`
- **What:** Defensive padding is good, but if fastembed ever returns
  a partial result set or an index >= n, the caller receives
  zero-padded scores that look indistinguishable from a real "this
  doc is irrelevant" signal. The retriever then sorts those at the
  bottom and silently drops them. Without a log line, this masks
  upstream library regressions.
- **Why:** The plan picked silent-drop for simplicity. Reasonable.
  Worth at least a `tracing::warn!` for the missing-index case so a
  fastembed bump that breaks the contract surfaces in logs.
- **Suggestion:** Track which positions were filled in a
  `Vec<bool>` of length `n`. After the loop, if any position is still
  unfilled, emit `tracing::warn!(missing = …, "rerank result missing
  positions; padded with 0.0")`. Same for indices >= n. No behavior
  change, just visibility.
- **Effort:** XS

### L5. `Indexer::run` re-extracts HEAD symbols even when the watermark is unchanged

- **Severity:** Low
- **Location:** `crates/ohara-core/src/indexer.rs:93-94`
- **What:** Tied to H2: `extract_head_symbols` is unconditionally
  called on every `run()`, regardless of whether HEAD has actually
  moved. The expensive path is the tree-sitter walk in
  `GitSymbolSource::extract_head_symbols`, which iterates the entire
  HEAD tree blob-by-blob. The CLI fast-path in
  `commands/index.rs:48-61` short-circuits the whole indexer when
  `incremental && watermark == HEAD`, but the indexer itself (when
  called from anywhere else, e.g. tests, or a future scripted
  caller) re-walks unconditionally.
- **Why:** Once H2 is fixed (e.g. `clear + put` inside
  `put_head_symbols`), re-walking is correctness-safe but still
  CPU-wasteful. Worth gating inside the indexer too.
- **Suggestion:** In `Indexer::run`, before `extract_head_symbols`,
  compare `latest_sha` against `status.last_indexed_commit` — if equal
  (i.e. no new commits indexed this pass), skip the symbol re-walk.
  Test: a repeat `Indexer::run` call with no new commits should
  leave `report.head_symbols == 0`.
- **Effort:** XS

### L6. `find_pattern_invokes_three_lanes_and_rrf` doesn't separately verify the RRF stage

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:434-486`
- **What:** The test attaches a `ScriptedReranker` whose scores
  override the RRF order entirely (diff-c=9 > diff-a=5 > diff-b=1), so
  the assertion `out[0].commit_sha == "c"` only confirms "reranker
  output dominates final order". It doesn't confirm the RRF stage
  picked, e.g., id 1 ahead of id 2 (which it should given lane shape).
  The "rerank wins over RRF" semantics are tested; the RRF lane
  ordering is not.
- **Why:** A bug where `find_pattern` skipped RRF entirely and just
  returned the union of lanes would still pass this test (because the
  reranker re-orders).
- **Suggestion:** Either tweak the existing test or add a sibling
  test where the reranker is order-preserving (returns a flat 1.0 for
  every candidate) and the assertion checks the surviving order
  matches the hand-computed RRF rank. Or simply assert call counts on
  `FakeStorage::calls` to confirm all three lanes ran (which the
  current test does — that's the "invokes_three_lanes" half of the
  name).
- **Effort:** XS

### L7. `with_no_rerank` builder method is dead code

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:88-91`
- **What:** Plan §1.6 mandated `with_no_rerank(self) -> Self`. It's
  implemented but no caller uses it (`grep` confirms zero call
  sites). The MCP path uses the `query.no_rerank` flag instead;
  callers wanting degraded mode just don't call `with_reranker`.
  The builder method is doc-load-bearing only.
- **Why:** Plan-mandated API. Either keep + document the use case,
  or delete + note the deviation.
- **Suggestion:** Keep it but add a doc-comment example showing when
  it's useful (e.g. tests, or a lifecycle where rerank is wired by
  default but a one-shot call wants to opt out without touching the
  per-call `PatternQuery`). Alternatively delete with a note in the
  next plan-errata pass.
- **Effort:** XS

### L8. Plan errata captured in plan §"Spec defects" never propagated back to the spec doc

- **Severity:** Low
- **Location:** `docs/superpowers/specs/2026-05-01-ohara-v0.3-retrieval-design.md`
  (silent on `HunkHit::hunk_id`, `Storage::clear_head_symbols`, async
  rerank wrapping, `--no-rerank` recency-still-applies semantics) vs.
  `docs/superpowers/plans/2026-05-01-ohara-plan-3-retrieval-pipeline.md`
  §"Spec defects spotted" (items 2-5)
- **What:** Pairs with M9 (item 1 of the same list). Items 2-5 are
  also still un-applied to the spec.
- **Why:** Same plan-spec drift problem; future readers using the
  spec as ground truth will rediscover the gaps.
- **Suggestion:** A single 5-bullet patch to the spec adding a
  "v0.3 implementation deviations" footer. No code change.
- **Effort:** XS (note)

### L9. `recency_weight` and `recency_half_life_days` aren't validated for non-positive values

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:184-186`
- **What:** If a caller constructs `RankingWeights { recency_half_life_days:
  0.0, .. }`, the recency factor becomes `exp(-age_days / 0.0)` →
  `exp(-inf)` → `0.0` for any positive age, `exp(NaN)` → `NaN` for
  age=0 (because 0.0 / 0.0 is NaN). NaN propagates into `combined`
  and ruins the sort (`partial_cmp` returns `None`, `unwrap_or(Equal)`
  → arbitrary order). Negative values produce inverted decay.
- **Why:** Defensive validation is cheap and makes constructor
  errors loud rather than producing silent wrong results. Default
  values are fine; the field is `pub`, so external `RankingWeights {
  ... }` constructions can hit this.
- **Suggestion:** Either add a `RankingWeights::new(...)` constructor
  that asserts `recency_half_life_days > 0.0`, or sanitize at use
  site (`max(self.weights.recency_half_life_days, f32::EPSILON)`).
- **Effort:** XS

### L10. `PatternHit::related_head_symbols` is hard-coded to `vec![]`

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:200`
- **What:** Field is on the JSON shape (visible to MCP clients) but
  always empty. Spec §"Component changes" doesn't promise content for
  v0.3; Plan 3 doesn't surface the chunked-symbol context to query
  results either. So it's an empty-by-design carryover from earlier
  plans, not a regression.
- **Why:** Mostly informational. If a future task ("show me which
  HEAD symbols co-locate with the matched hunk") fills it in, the
  field name + position is already pinned.
- **Suggestion:** No action for v0.3.0. When the symbol-context
  surfacing lands (Plan 4-ish), wire it via the existing field;
  meanwhile note in retriever.rs that the field is reserved.
- **Effort:** XS (when triggered)

---

### See also

- `cargo test --workspace` — 69 passed / 9 ignored / 0 failed at HEAD
  `0ad33bb` (78 total; plan claimed 80+).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all --check` — clean (the final commit `0ad33bb` ran
  `cargo fmt`).
- Plan-1 carry-overs all still verified by Task 19's backlog;
  nothing regressed in this work.
- Time-sensitive: H1, H2, H3 before tagging v0.3.0. M1, M3, M5 in the
  same window if the API surface or correctness of nested-language
  parsing matters for the release. Everything else can wait.
- Plan-aware: a follow-up "Plan 3.1" patch landing H1+H2+H3 plus the
  three M1/M3/M5 items is the natural shape; M2/M6/M7/M8/M9 fold in
  if the patch grows.
