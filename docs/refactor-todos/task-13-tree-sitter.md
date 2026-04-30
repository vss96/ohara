# Task 13: tree-sitter symbol extraction — refactor backlog

Captured at HEAD `ef2df7a`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–12 backlogs are not duplicated here. Items in Task 14+ proper scope
(Retriever, CLI wiring) are out of scope here.

---

### 1. Plan-vs-implementation drift in `python.rs` extraction logic

- **Severity:** Low
- **Location:** `crates/ohara-parse/src/python.rs:18-115` vs Task 13 plan.
- **What:** The plan listed a state-machine that emitted exactly one
  symbol per match; in reality, a single `class_definition` match
  carries both class and inner-method captures, and the top-level
  `function_definition` pattern double-fires on methods. Implementer
  rewrote the loop to emit 1–2 symbols per match and dedupe by span,
  preferring Method/Class over Function. The plan doc is now stale.
- **Why:** Future re-readers will be confused by the divergence.
- **Suggestion:** Add a one-paragraph errata note in the Task 13 plan
  pointing at this file. No code change.
- **Effort:** XS

### 2. `tree.walk` swallows per-file extraction errors silently

- **Severity:** Medium
- **Location:** `crates/ohara-parse/src/lib.rs:48-58`.
- **What:** Inside the `tree.walk` callback, `find_blob`, `from_utf8`,
  and `extract_for_path` are each gated by `if let Ok(...)`. Any
  failure (parse error on a malformed source file, non-UTF8 content,
  blob lookup failure) is dropped on the floor. The walk continues,
  but we never know which files were skipped or why.
- **Why:** When indexing real repos, malformed sources or vendor dirs
  containing non-UTF8 bytes will silently produce missing symbols.
  Diagnosing "why is `MyClass` not in the index?" becomes archaeology.
- **Suggestion:** Replace the `if let Ok` chain with explicit
  `match`es that emit `tracing::warn!(path = %p, err = %e, "...")`
  on each failure mode. Keep the walk going (don't `?`) — log + skip.
- **Effort:** S

### 3. No `tracing` instrumentation in `extract_head_symbols`

- **Severity:** Low
- **Location:** `crates/ohara-parse/src/lib.rs:33-65`.
- **What:** Neither `extract_head_symbols` nor the `spawn_blocking`
  body carry `#[tracing::instrument]` or debug events. No counts of
  files walked, files parsed, files skipped, or symbols extracted.
- **Why:** Same rationale as Task 11 #4 and Task 12 #6 (continuation
  of that pattern): cheap up-front, painful to retrofit when an
  indexer pass takes minutes against a large repo and we want to
  know whether parsing or git I/O dominates.
- **Suggestion:** `#[tracing::instrument(skip(self))]` on the impl
  method; inside the closure, `tracing::debug!(files = ..., symbols
  = ...)` before returning. Cross-reference Task 11 #4, Task 12 #6.
- **Effort:** XS

### 4. Per-match `HashMap` allocation in `python.rs` is hot-loop overhead

- **Severity:** Low
- **Location:** `crates/ohara-parse/src/python.rs:99-115`.
- **What:** The dedup `HashMap<(u32, u32), Symbol>` allocates once
  per file and holds owned `Symbol` values; `into_values` also
  reallocates. Not a measured bottleneck — flag for awareness.
- **Why:** Symbol extraction runs on every indexed file. 10k Python
  files at 50 symbols each adds map churn. Worth a micro-bench
  before optimising.
- **Suggestion:** Sort `out` by `(span_start, span_end, kind_priority)`
  and dedupe in place, or use a `HashSet<(u32, u32)>` of seen
  Method/Class spans and filter Function entries in a second pass.
  Defer until a profiler points here.
- **Effort:** S

### 5. Dedup key `(span_start, span_end)` lacks `file_path`

- **Severity:** Low
- **Location:** `crates/ohara-parse/src/python.rs:99-114`.
- **What:** Dedup is per-file (the `HashMap` lives inside `extract`
  which is called per file), so the omission is harmless today.
  Recording for awareness in case the function is ever inlined into
  a multi-file pass that shares one map across files — two files
  with a method at the same byte range would collide.
- **Why:** Defensive: the function signature suggests per-file scope,
  but a future refactor could batch files for performance and
  inadvertently break the invariant.
- **Suggestion:** Either keep the dedup strictly per-file (current
  contract) and add a `// per-file dedup` doc comment, or include
  `file_path` in the key for futureproofing. Doc comment is cheaper.
- **Effort:** XS

### 6. tree-sitter version pin is loose (`"0.22"` allows minor bumps)

- **Severity:** Low
- **Location:** workspace `Cargo.toml:37-39`.
- **What:** `tree-sitter = "0.22"`, `tree-sitter-rust = "0.21"`,
  `tree-sitter-python = "0.21"` resolve as `^` ranges. The v0.22
  → v0.23 API rename (`LANGUAGE` const vs `language()` fn) forced
  this pin; another such break during `cargo update` would be silent.
- **Why:** We're one minor bump away from another `language()` -style
  surprise.
- **Suggestion:** Tighten to `=0.22` / `=0.21` until v1 ships;
  schedule upgrade post-v1. Document pin reason in a Cargo.toml comment.
- **Effort:** XS

### 7. Python query is brittle to decorators, nested classes, docstrings

- **Severity:** Low
- **Location:** `crates/ohara-parse/queries/python.scm`.
- **What:** The `class_definition.body` matches a `block` whose
  immediate children are `function_definition` — but real classes
  carry decorated methods (`decorated_definition` wraps the
  `function_definition`) and nest classes. Decorated methods will
  not be captured; nested classes' methods attribute to the wrong
  outer class.
- **Why:** Real-world Python coverage is meaningfully worse than
  the test fixture suggests. Embedding-search will miss decorated
  methods (most of FastAPI/Pydantic).
- **Suggestion:** Match `(decorated_definition definition:
  (function_definition ...))` as a method alternative; add fixtures
  for `@property`, `@classmethod`, dataclass, nested class. Defer
  to a follow-up Task.
- **Effort:** M

### 8. JS/TS/Go languages deferred — language dispatch is an `if`-ladder

- **Severity:** Low
- **Location:** `crates/ohara-parse/src/lib.rs:10-16`.
- **What:** Spec §10 lists v1 languages as Rust, Python, TypeScript,
  JavaScript, Go. We ship Rust + Python only (intentional, per plan
  §3 deferral). The dispatch is a hand-coded `match` on file
  extension; adding three more languages will balloon both this
  function and the module list. No registry / trait-object pattern.
- **Why:** Recording the gap so it isn't forgotten when post-v1
  language additions land. Plan §3 calls this out as post-v1.
- **Suggestion:** When the second-or-third language lands, refactor
  to a `LanguageExtractor` trait + `static REGISTRY: &[(ext, fn)]`
  table. Don't do it now — premature abstraction with N=2.
- **Effort:** S (when triggered)

---

### See also

- `cargo clippy -p ohara-parse --all-targets` is clean at HEAD.
- Plan-aware: Task 14 (Retriever) doesn't consume `ohara-parse`.
  Task 15 (CLI) instantiates `GitSymbolSource`, so #2 and #3 become
  user-visible the moment a real repo is indexed. Address before
  the first end-to-end CLI run.
- Time-sensitive: #2, #3 (before first large-repo indexing run);
  #6 (tighten pin) before the next `cargo update`. Anytime: rest.
- Cross-task: #3 continues Task 11 #4 and Task 12 #6 — same pattern
  across crates.
- Spec drift: §10 v1 language list (Rust, Python, TS, JS, Go) is
  intentionally partial; #8 tracks the post-v1 follow-up.
