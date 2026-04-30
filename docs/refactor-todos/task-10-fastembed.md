# Task 10: fastembed provider — refactor backlog

Captured at HEAD `e3c59ed`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–9 backlogs are not duplicated here.

---

### 1. `.fastembed_cache/` leaks into untracked status

- **Severity:** High
- **Location:** root `.gitignore` (missing entry); cache appears at
  `crates/ohara-embed/.fastembed_cache/` once the live test runs.
- **What:** The `--include-ignored` test downloads BGE-small (~80 MB)
  into `.fastembed_cache/` cwd-relative. Shows up as untracked for
  every contributor who runs the live test; risks accidental `git add .`.
- **Why:** Pollutes `git status` permanently; ~80 MB binary artefacts
  are not something we want a stray `git add` to slurp. Spec §9 says
  cache should live at `~/.ohara/models/`; until that's wired, ignore.
- **Suggestion:** Add `**/.fastembed_cache/` to root `.gitignore`.
  Separately file a follow-up to make `FastEmbedProvider::new` honour
  `~/.ohara/models/` per spec §9.
- **Effort:** XS

### 2. `Mutex<TextEmbedding>` is defensive — document or remove

- **Severity:** Medium
- **Location:** `crates/ohara-embed/src/fastembed.rs:11,38-42`
- **What:** Plan v1 expected `embed` to take `&mut self`; the shipped
  fastembed v4 API takes `&self`, so the lock isn't required for
  correctness. `Arc<TextEmbedding>` with concurrent `embed()` would
  compile and run. The lock was kept defensively without a comment.
- **Why:** Future readers spend time deriving why the lock exists; could
  get refactored away by mistake or kept forever as cargo-cult.
- **Suggestion:** Either drop to `Arc<TextEmbedding>` with a property-test
  that two concurrent `embed_batch` calls return identical vectors, or
  add a `// RATIONALE:` comment naming the upstream `&mut self`
  protection and ONNX-session serialisation as the reason.
- **Effort:** XS (document) / S (remove + test)

### 3. `tokio::sync::Mutex` inside `spawn_blocking` — `std::sync::Mutex` fits better

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:5,39`
- **What:** `tokio::sync::Mutex` is designed to hold a guard across `.await`
  points. Here the guard is acquired inside `spawn_blocking` (no awaits
  while held) and released before the closure returns. `std::sync::Mutex`
  (or `parking_lot::Mutex`) fits the access pattern, avoids the
  `blocking_lock` panic risk if ever called from a runtime thread by
  mistake, and is marginally faster.
- **Why:** Subtle correctness footgun: `blocking_lock` panics on a Tokio
  runtime thread. Today only `spawn_blocking` calls it, but a future
  caller refactoring this method could trip it. `std::sync::Mutex::lock`
  has no such gotcha here.
- **Suggestion:** Swap to `std::sync::Mutex` (drop the `tokio` dep from
  this module) when revisiting #2. If #2 lands as "remove the lock", this
  becomes moot.
- **Effort:** XS (bundle with #2)

### 4. `#[ignore]` test is the only fastembed integration coverage

- **Severity:** Medium
- **Location:** `crates/ohara-embed/src/fastembed.rs:54-63`; CI workflow
  (none yet — flag for whichever PR introduces CI).
- **What:** Default `cargo test` skips the live test. A future fastembed
  dep bump (model-id rename, `InitOptions` builder change like the one
  this task already worked around, etc.) can silently break
  `FastEmbedProvider::new` / `embed_batch` and only surface in Task 14's
  e2e run. Dependabot PRs would go green on broken embedder.
- **Why:** Cheap insurance against silent dep-bump breakage. The 80 MB
  download is one-time-per-runner if cached.
- **Suggestion:** Add a CI job that runs
  `cargo test -p ohara-embed -- --include-ignored` with the model cache
  persisted across runs, triggered on `Cargo.lock` changes or weekly.
  Defer the workflow file to whichever task introduces CI.
- **Effort:** S (when CI lands)

### 5. No module-level doc on `fastembed.rs`

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:1`
- **What:** Same gap Task 8 #7 / Task 9 #4 flagged elsewhere. Module
  wraps non-trivial decisions (sync ONNX in `spawn_blocking`, hardcoded
  model id, lock rationale) with no `//!`. Crate-level `lib.rs` doc is
  one line.
- **Why:** Future authors picking alt providers (post-v1 voyage-code-3)
  need lock + `spawn_blocking` rationale up-front.
- **Suggestion:** Add `//!` covering: BGE-small default (spec §9),
  `spawn_blocking` rationale (CPU-bound ONNX), Mutex rationale (resolves
  with #2), model-id hardcoded for v1 (resolves with #7).
- **Effort:** XS

### 6. `TextEmbedding::try_new` error is opaque

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:24`
- **What:** `try_new` can fail for distinct reasons (network during
  first-run download, disk-full, corrupt cache, ONNX init, HF auth).
  All bubble up through `?` as a single `anyhow::Error` with whatever
  message fastembed produced, often just "request failed".
- **Why:** First-run UX (offline at first launch) is the most common
  failure mode and the user gets no actionable next step.
- **Suggestion:** Wrap with `.context("loading BGE-small model from
  fastembed cache or HuggingFace")`. When spec §9 model-cache lands,
  add a pre-flight cache-existence check.
- **Effort:** XS (context) / S (pre-flight)

### 7. `model_id` and `dim` are hardcoded, not constructor params

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:7-8,17-26`
- **What:** `FastEmbedProvider::new()` accepts no arguments. Switching
  to any other fastembed model requires editing source. Spec §9
  anticipates `voyage-code-3` post-v1, and the trait exists for exactly
  that reason.
- **Why:** Post-v1, but the second model that gets added will retrofit
  configurability under pressure; cheaper to design the seam now.
- **Suggestion:** Add `FastEmbedProvider::with_model(EmbeddingModel) ->
  Result<Self>`; keep `new()` as an alias. Resolve model-id and dim
  from the `EmbeddingModel` rather than constants.
- **Effort:** S

### 8. Cold-start cost (1–2s ONNX init + first-run ~80 MB) is invisible

- **Severity:** Low
- **Location:** `crates/ohara-embed/src/fastembed.rs:17-26`; surfaces in
  Task 14 / MCP wiring.
- **What:** `TextEmbedding::try_new` does ONNX session init synchronously
  (1–2s warm cache, 10–60s cold). Eager construction at MCP server boot
  blocks startup; lazy makes the first `find_pattern` slow.
- **Why:** Pre-emptive note for Task 14 / MCP boot — not actionable in
  this crate alone but a decision the wiring task must make.
- **Suggestion:** Flag as input for Task 14 / MCP binary task: choose
  eager-with-readiness-gate vs. lazy vs. background-warm, document the
  choice, and add a `tracing::info!("fastembed: model loaded in {ms}ms")`
  once a logging target is wired.
- **Effort:** XS (note) / S (handle in Task 14)

---

### See also

- `cargo clippy -p ohara-embed --all-targets` is clean at HEAD.
  Pre-existing warnings in `ohara-core` (`unused imports`, `dead_code`
  on `Indexer.embed_batch` and `Retriever.{storage,embedder}`) belong to
  Task 3–4 backlog, not here.
- Spec drift: model cache lives in `crates/ohara-embed/.fastembed_cache/`
  (cwd-relative) instead of `~/.ohara/models/` per spec §9 — captured as
  the trigger for #1's follow-up, owned by reviewer for the spec note.
- Trait shape (`embed_batch(&self, texts: &[String])` vs. spec's
  `&[&str]`) is a pre-existing core-crate decision, not a Task 10 item.
- Time-sensitive: #1 (every contributor hits this), #4 (before first dep
  bump), #7 (before Task 14 / second model). Anytime: #2, #3, #5, #6, #8.
