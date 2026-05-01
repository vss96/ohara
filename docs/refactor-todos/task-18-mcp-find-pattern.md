# Task 18: MCP `find_pattern` tool — refactor backlog

Captured at HEAD `cfdce27`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in Tasks
6–17 backlogs are not duplicated, only cross-referenced. Task 19+ proper scope
is out of scope. `cargo clippy -p ohara-mcp --all-targets` is clean.

---

### 1. `OharaServer::embedder` field is dead-code-only — drop it

- **Severity:** High
- **Location:** `crates/ohara-mcp/src/server.rs:11-14, 37`
- **What:** Field is `#[allow(dead_code)]` with a doc-comment saying "Kept
  alive so the FastEmbed model held inside `retriever` stays loaded."
  But `Retriever::new` already takes `Arc<dyn EmbeddingProvider>` and
  clones it; the model lives as long as `retriever` does. The field is
  redundant.
- **Why:** A load-bearing `allow(dead_code)` *built on a misconception*
  teaches the next reader the wrong lifetime model. Task 17 #8 said all
  four `allow(dead_code)` markers should disappear with Task 18; this
  is the survivor.
- **Suggestion:** Delete the field, move the `Arc` directly into
  `Retriever::new`, drop the `EmbeddingProvider` import.
- **Effort:** XS

### 2. rmcp pinned `^0.1`, but plan-verbatim code only compiles on `=0.1.5`

- **Severity:** High
- **Location:** `Cargo.toml:41`, consumed by `tools/find_pattern.rs`
- **What:** Implementer flagged six API adaptations forced by rmcp 0.1.5
  (no `tool_router` macro, different `Error` constructors, etc.). `^0.1`
  caret silently accepts `0.1.6+`; rmcp pre-1.0 makes no stability
  promises. Macros (`tool(tool_box)`, `tool(aggr)`) and the manual
  `ServerHandler` impl moved across recent minors.
- **Why:** Same failure mode Task 10 hit with fastembed — pre-1.0 dep
  drifted under `cargo update`, build broke with no code change. Task 10
  fixed it by pinning `=4.9.x`.
- **Suggestion:** Pin `rmcp = { version = "=0.1.5", ... }`. One-line
  comment citing plan-errata.
- **Effort:** XS

### 3. Plan-errata for the six rmcp 0.1.5 adaptations isn't recorded

- **Severity:** High
- **Location:** `docs/superpowers/plans/2026-04-30-ohara-plan-1-foundation-and-find-pattern.md`
  Task 18 (~L3506); referenced from `find_pattern.rs:4-5`
- **What:** Implementer adapted 6 call sites away from plan-verbatim.
  Module doc-comment hand-waves "see the report"; the plan still shows
  the wrong syntax. Plan 2's `explain_change` will reuse this exact
  `OharaService`/`ServerHandler` shape.
- **Why:** Errata-in-PR-report is invisible to anyone not reading PR
  history. Pairs with #2.
- **Suggestion:** Add a "Plan-errata" subsection at the top of the
  Task 18 block listing each deviation (plan said X, ship says Y).
  Cross-reference from the module doc-comment.
- **Effort:** S

### 4. `find_pattern` handler has no `tracing::instrument`

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/tools/find_pattern.rs:71-99`
- **What:** Four awaitable operations (parse_since, knn,
  index_status_meta, json encode) with no spans, no per-stage events.
  Continues structured-tracing thread from Tasks 11 #4, 12 #6, 13 #3,
  14 #7, 15 #4, 17 #5.
- **Why:** MCP transport is stdio — stderr tracing is *the only*
  debug signal. First user-facing handler in ohara.
- **Suggestion:** `#[tracing::instrument(skip(self), fields(query =
  %input.query, k = input.k))]`; `tracing::debug!(stage = ...)` per
  step; `tracing::warn!` on error-map paths.
- **Effort:** XS

### 5. Handler error mapping collapses every backend error to `internal_error`

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/tools/find_pattern.rs:84-94`
- **What:** Both `retriever.find_pattern` and `index_status_meta` use
  `.map_err(|e| rmcp::Error::internal_error(...))`. But
  `OhraError::RepoNotIndexed` (`ohara-core/src/error.rs:17-18`) is the
  *expected* "user must run `ohara index`" path; `internal_error` makes
  clients treat a recoverable condition as a server bug.
  `InvalidArgument` has the same issue.
- **Why:** `invalid_params` vs `internal_error` changes how Claude/Cursor
  presents the failure. `_meta.hint` only fires when the call succeeds
  far enough to compute meta — `RepoNotIndexed` during retrieval
  bypasses it.
- **Suggestion:** Match on `OhraError` — `RepoNotIndexed` →
  `invalid_params` with hint; `InvalidArgument` → `invalid_params`;
  rest → `internal_error`. Inline now; promote to `tools/error.rs` when
  `explain_change` (Plan 2) needs the same map.
- **Effort:** S

### 6. Tool response is `Content::text(json.to_string())` — investigate structured

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/tools/find_pattern.rs:96-97`
- **What:** Payload is serialized to a JSON *string* and shipped as
  `Content::text`. MCP defines structured content; Claude Code renders
  `Content::text` verbatim — user sees a wall of escaped JSON.
- **Why:** First Claude Desktop impression of ohara is "the tool dumped
  JSON at me." Plan's Layer 4 theme: response format *is* UX.
- **Suggestion:** Audit rmcp 0.1.5's `Content` variants for a JSON /
  structured variant; the `=0.1.5` pin (#2) gates this. If absent,
  document and block on upgrade.
- **Effort:** S / M (if blocked upstream)

### 7. `TOOL_DESCRIPTION` / `SERVER_INSTRUCTIONS` are production prompt copy

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/tools/find_pattern.rs:16-36`
- **What:** Both constants are LLM-facing prompt copy — how Claude
  decides whether to invoke ohara. Plan says "tool description IS
  production code" but the constants sit next to the handler with no
  marker. Plan 3's `ohara init` writes CLAUDE.md (Layer 3 of the same
  discoverability stack); Plan 2 adds a second pair for
  `explain_change`.
- **Why:** A casual "tighten the wording" edit can measurably change
  tool-call rate. Blast radius invisible from the file.
- **Suggestion:** (a) Add a `// PROMPT-COPY: changes affect tool
  selection rate; coordinate with CLAUDE.md (Plan 3).` comment.
  (b) When Plan 3 lands, consolidate to `discoverability.rs` owning
  constants + CLAUDE.md template generator.
- **Effort:** XS / S (gated on Plan 3)

### 8. No handler-level test; only `parse_since` is covered

- **Severity:** Low
- **Location:** `crates/ohara-mcp/src/tools/find_pattern.rs:130-149`
- **What:** Tests cover only `parse_since`. The handler — `k.clamp(1,
  20)`, now-vs-since wiring, meta-attachment, future error mapping (#5)
  — has no in-crate test. Task 20 e2e covers success through stdio
  (slow: real model, real git).
- **Why:** A handler-level test with stub `Storage`/`EmbeddingProvider`
  runs in ms — fastest signal for #5 + clamp regressions. Helper-tests
  thread now hits five call sites (Tasks 11, 13, 15, 17 #5).
- **Suggestion:** Add `find_pattern_clamps_k`,
  `find_pattern_propagates_meta_hint`, and (after #5)
  `find_pattern_maps_repo_not_indexed_to_invalid_params`. Reuse
  `ohara-core` stubs (cf. Task 14).
- **Effort:** S

---

### See also

- `cargo clippy -p ohara-mcp --all-targets` — clean at HEAD `cfdce27`.
- Inherited from Task 17 (intersect here): #1 (`commits_behind` extract,
  `index_status_meta` now hot-path); #2 (shared `OHARA_HOME`); #3 (lazy
  embedder — `find_pattern` is the paying consumer; item #1 above
  simplifies the OnceCell migration); #5 (`instrument` on `open` —
  pairs with item #4); #6 (graceful shutdown, now operationally
  relevant); #7 (`async-trait` dep — re-verify post-Task-18).
- Plan-aware: Plan 2 `explain_change` grows `OharaService` (#5, #7
  pre-pay). Plan 3 `ohara init` writes CLAUDE.md (trigger for #7).
  Task 20 e2e exercises this handler and surfaces #6 in real hosts.
- Time-sensitive: #1, #2, #3 before Plan 2 starts. #4, #5, #8 anytime.
  #6, #7 paired with downstream plans.
