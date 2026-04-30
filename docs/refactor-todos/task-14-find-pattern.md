# Task 14: Retriever.find_pattern — refactor backlog

Captured at HEAD `1928689`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–13 backlogs are not duplicated here. Items in Task 15+ proper scope
(CLI wiring, MCP tool surface) are out of scope here.

---

### 1. `cosine` silently truncates on length mismatch

- **Severity:** Medium
- **Location:** `crates/ohara-core/src/retriever.rs:115-120`
- **What:** `cosine(a, b)` uses `a.iter().zip(b.iter())`, so unequal-length
  vectors produce a result over the shorter prefix without any signal. In
  practice the query embedding and message embeddings share a `dimension()`
  and this can't drift — but a future bug in the embedder (model swap,
  partial response) would be invisible here and silently bias scores.
- **Why:** Embedding-dimension invariants are exactly the kind of thing a
  cheap assertion should defend. The cost is one debug-build check; the
  payoff is loud failure instead of silently-wrong rankings.
- **Suggestion:** `debug_assert_eq!(a.len(), b.len(), "cosine: dim mismatch");`
  at the top of `cosine`. Optional: also assert `a.len() == self.embedder.dimension()`
  in `find_pattern` once per call.
- **Effort:** XS

### 2. `q_text = vec![...]` then `q_embs.pop()` is awkward indirection

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:90-92`
- **What:** Building a one-element `Vec<String>` to call `embed_batch` then
  `pop()`-ing reads as boilerplate around what is logically a single-text
  embed. `pop()` returns the *last* element; correct here but mildly
  surprising to readers.
- **Why:** Minor readability. The `Embedding("empty")` error string is
  also unhelpful — it can only fire on an embedder contract violation.
- **Suggestion:** `q_embs.into_iter().next().ok_or_else(...)` reads more
  intentionally. Or add an `EmbeddingProvider::embed_one` default method;
  defer that trait change until a second caller appears.
- **Effort:** XS

### 3. `FakeEmbedder` matches on string literals — fragile test surface

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:284-291`
- **What:** The fake embedder hardcodes vectors for three exact strings
  and returns zero vectors otherwise. New tests adding a new query string
  silently get `cosine = 0.0`, masking real ranking bugs.
- **Why:** As find_pattern's test surface grows (Task 18 MCP tests, Task 20
  e2e), the brittleness compounds. A new test author has to read the
  fake's match arms to understand why their query "doesn't work."
- **Suggestion:** When the surface grows past ~3 queries, switch to
  `mockall` or a deterministic "hash text → normalised vec" embedder.
  For now, a doc comment listing supported inputs suffices.
- **Effort:** S (when triggered)

### 4. `FakeStorage::knn_hunks` ignores `k`, `language`, `since_unix`

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:303`
- **What:** The fake returns `self.hits.clone()` regardless of filter args.
  `find_pattern` clamps `k` to `1..=20` and forwards `language`/`since_unix`
  to storage; none of that wiring is exercised by the existing test.
- **Why:** A future change to the clamp range or filter forwarding won't
  be caught here. Test is honest about what it verifies (tiebreak), but
  the gap is worth recording.
- **Suggestion:** Add a second test that captures the call args in a
  `Mutex<Vec<...>>` and asserts forwarding. Cheap; locks down the contract.
  Or rely on Task 20 e2e — but unit-level is preferable.
- **Effort:** S

### 5. Empty-query semantics are undefined

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:84-92`
- **What:** `find_pattern` with `query.query = ""` calls
  `embed_batch(&[""])`. fastembed will return *some* vector (model-dependent,
  typically a non-zero "pad/CLS" embedding), which then drives a KNN search
  whose results are nonsense. There's no guard, no doc, no test.
- **Why:** Task 18 (MCP) will expose `find_pattern` to LLM-generated
  queries. An empty or whitespace-only query from a confused caller
  becomes a silent low-quality result instead of a clear error.
- **Suggestion:** At the top of `find_pattern`, `if query.query.trim().is_empty()
  { return Err(OhraError::InvalidInput("empty query".into())); }`. Or
  return `Ok(vec![])`. Document the choice in the function's rustdoc.
- **Effort:** XS

### 6. Two `embed_batch` calls per query — could be one

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:91, 108`
- **What:** Two sequential `embed_batch` awaits: once for the query, once
  for candidate messages. The second can't start until KNN returns. A
  single batched call (`[query, ...messages]`) would amortise warm-up,
  but requires reordering: KNN needs the query embedding first.
- **Why:** Not a bottleneck today; matters once a remote embedder lands
  (round-trip cost) or k grows. Local fastembed's batch path is already
  optimised; the duplication is mildly wasteful.
- **Suggestion:** Defer. If profiling points here, restructure so the
  query is embedded first (KNN dependency), then messages alone in one
  call — already the current shape. Real win only with caching across
  calls; track that separately.
- **Effort:** S

### 7. `find_pattern` lacks `tracing` instrumentation

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:84-113`
- **What:** No `#[tracing::instrument]`, no debug events. We don't log
  `k`, candidate count, time spent in `embed_batch` vs `knn_hunks` vs
  ranking. Continues the pattern flagged in Task 11 #4, Task 12 #6,
  Task 13 #3.
- **Why:** Task 18 (MCP) and Task 20 (e2e) will exercise this against
  real fastembed and real sqlite-vec. When a query is slow or returns
  zero hits, we'll want a breakdown without breaking out a profiler.
- **Suggestion:** `#[tracing::instrument(skip(self, query), fields(repo = %repo_id, k = query.k))]`
  on `find_pattern`; `tracing::debug!(candidates = candidates.len(), "knn returned")`
  after the storage call. Cross-reference Task 11 #4, Task 12 #6, Task 13 #3.
- **Effort:** XS

### 8. Two `impl Retriever` blocks — should be merged

- **Severity:** Low
- **Location:** `crates/ohara-core/src/retriever.rs:26-81` and `83-113`
- **What:** Task 14 added `find_pattern` in a second `impl Retriever`
  block rather than appending to the existing one. Compiler doesn't
  care, but it splits methods that conceptually belong together and
  makes a future `cargo doc` page show them in two groups.
- **Why:** Pure tidiness. Likely an artefact of the implementer keeping
  the new code physically isolated for review-diff legibility.
- **Suggestion:** Merge the two `impl` blocks. One-line change, do it
  during the next touch of this file.
- **Effort:** XS

---

### See also

- `cargo clippy -p ohara-core --all-targets` — 3 warnings remain (unused
  `IndexStatus` import in `indexer.rs:1`, unused `CommitMeta`/`Hunk` lib-level
  imports in `retriever.rs:3` due to test-only use, dead `embed_batch` field
  in `indexer.rs:27`). All inherited from earlier tasks; covered by the
  Task 4 backlog. Worth a one-shot cleanup pass after Plan 1 ships.
- Plan-aware: Task 18 (MCP `find_pattern` tool) calls this method
  directly — items #5 (empty query) and #7 (tracing) become
  user-visible the moment the MCP surface lands. Item #1 (cosine assert)
  becomes load-bearing if a remote embedder is ever swapped in.
- Plan-aware: Task 20 (e2e) exercises the full `find_pattern` path
  against real fastembed + sqlite-vec; items #4 (filter coverage) and
  #6 (batch consolidation) get free coverage there.
- Time-sensitive: #5 before Task 18 ships; #7 alongside Task 18.
  Anytime: rest.
- Cross-task: #7 continues Task 11 #4, Task 12 #6, Task 13 #3 — same
  pattern across crates.
