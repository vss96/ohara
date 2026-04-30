# Task 17: `ohara-mcp` server skeleton — refactor backlog

Captured at HEAD `38268ce`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in
Tasks 6–16 backlogs are not duplicated here, only cross-referenced. Task 18's
proper scope (real rmcp wiring + `find_pattern` handler) is out of scope.
`cargo clippy -p ohara-mcp --all-targets` is clean at this HEAD.

---

### 1. `OharaServer::index_status_meta` byte-for-byte duplicates `status::run`

- **Severity:** High
- **Location:** `crates/ohara-mcp/src/server.rs:46-66` mirrors
  `crates/ohara-cli/src/commands/status.rs:19-24`
- **What:** The `match st.last_indexed_commit { Some(sha) => list_commits(...).len(),
  None => list_commits(None).len() }` block is now duplicated verbatim across
  both binaries. Task 16 #5 flagged this as a "when Task 17 lands" trigger;
  the trigger has now fired.
- **Why:** Any change to the "behind HEAD" definition (excluding merges,
  capping for huge repos, switching to revwalk-count per Task 16 #4) must
  be made twice. The hint thresholds (`behind > 50`, the "not built" copy)
  also belong in one place so the CLI's `--json` mode (Task 16 #2) and the
  MCP `ResponseMeta` agree.
- **Suggestion:** Extract `pub fn commits_behind(walker: &GitWalker,
  watermark: Option<&str>) -> Result<u64>` in `ohara-git`, plus
  `index_status_with_hint(...)` on `ohara-core::query`. Call from both.
  Pairs with Task 16 #4 + #5.
- **Effort:** S

### 2. `OHARA_HOME` resolution duplicated across CLI and MCP binaries

- **Severity:** High
- **Location:** `crates/ohara-mcp/src/server.rs:25-29` mirrors
  `crates/ohara-cli/src/commands/mod.rs:9-15`
- **What:** The `OHARA_HOME` → `HOME` → `USERPROFILE` lookup, the
  `.ohara` join, and the `<home>/<repo_id>/index.sqlite` path assembly
  are duplicated. The MCP copy also inherits the same `.expect("HOME")`
  panic flagged in Task 15 #1.
- **Why:** Task 15 #1 (panic → typed error) and the "extract `ohara_home`
  to a shared crate" thread both compound here. Three call sites
  (`index`, `query`/`status`, `mcp`) is the canonical extract trigger.
- **Suggestion:** Promote `ohara_home() -> Result<PathBuf>` and
  `index_db_path(&RepoId) -> Result<PathBuf>` to a shared module —
  `ohara-core::paths` is the natural home. Migrate both binaries.
  Resolves Task 15 #1 in the same change.
- **Effort:** S

### 3. `OharaServer::open` does network-bound work synchronously at startup

- **Severity:** High
- **Location:** `crates/ohara-mcp/src/server.rs:32-34`
- **What:** `FastEmbedProvider::new()` may download BGE-small ONNX weights
  (~30 MB) on first run. `open` runs before `serve_stdio`, so an MCP
  client connecting to a fresh install waits for the download with no
  signal. MCP clients enforce connect timeouts (Claude Desktop is ~30 s);
  a slow network blows past it.
- **Why:** First-run UX becomes "the server silently doesn't work." The
  rest of `open` succeeds in milliseconds — the embedder is the only
  slow stage and is not needed until the first `find_pattern` call.
- **Suggestion:** Lazy-init the embedder behind a `tokio::sync::OnceCell`
  on `OharaServer`. First `find_pattern` pays the cost; the MCP handshake
  returns immediately. Optional: surface a `setup_in_progress` state in
  `ResponseMeta.hint`. Pairs with Task 18.
- **Effort:** S (OnceCell) / M (status surface)

### 4. `OharaServer::open` mixes four IO concerns into one method

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/server.rs:19-38`
- **What:** Canonicalize → git discover → home/db-path resolve → storage
  open → embedder init → `Retriever::new` all happen in one ~20-line
  method with no seams. Testing any individual stage requires a real
  repo plus a real model.
- **Why:** Task 20 e2e covers this end-to-end, fine. Cost surfaces when
  (a) Task 18 wants to inject a test embedder, (b) #3's lazy-init needs
  to slot in cleanly, (c) `RepoHandle` from Task 15 #7 lands.
- **Suggestion:** Split into private stages: `discover_repo`,
  `open_storage`, `init_embedder`, then assemble. Defer to Plan 2
  unless #3 forces the split sooner.
- **Effort:** S

### 5. No `tracing::instrument` on the multi-stage `OharaServer::open`

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/server.rs:19-38`
- **What:** Four distinct IO stages run with no spans and no per-stage
  events. When the first-run embedder download stalls (#3) or storage
  opens against a corrupt sqlite, the operator sees no breadcrumbs —
  just a hung future or a one-line context-wrapped error.
- **Why:** Continues the structured-tracing thread from Task 11 #4,
  Task 12 #6, Task 13 #3, Task 14 #7, Task 15 #4. The MCP server is the
  first long-lived ohara process; tracing is now load-bearing.
- **Suggestion:** `#[tracing::instrument(skip_all, fields(repo = %workdir...))]`
  on `open`; one `tracing::info!(stage = "...")` per stage. Same for
  `index_status_meta`.
- **Effort:** XS

### 6. `main` has no graceful shutdown — Ctrl-C kills mid-request

- **Severity:** Medium
- **Location:** `crates/ohara-mcp/src/main.rs:6-16`
- **What:** `serve_stdio().await` runs to completion; no
  `tokio::signal::ctrl_c()` race, no shutdown channel. WAL + sqlx make
  a Ctrl-C safe today, but it's a polish gap and a footgun for any
  future in-flight write that isn't a single transaction.
- **Why:** Long-lived process; users will Ctrl-C it. Task 18 adds the
  actual rmcp transport which is the natural place to wire the signal.
- **Suggestion:** `tokio::select! { _ = serve_stdio() => ..., _ =
  tokio::signal::ctrl_c() => ... }` plus explicit storage drop before
  exit. Defer cancellation-token propagation into handlers to Task 18.
- **Effort:** XS

### 7. `async-trait` and `chrono` declared in `Cargo.toml` but unused

- **Severity:** Low
- **Location:** `crates/ohara-mcp/Cargo.toml:25-26`
- **What:** Task 17 added `async-trait = "0.1"` and `chrono.workspace
  = true` but neither is referenced anywhere in `src/`. Presumably
  staged for Task 18's rmcp tool-trait macros and timestamp handling,
  but `cargo udeps` would flag them today.
- **Why:** Speculative deps drift unnoticed. If Task 18's design changes
  (rmcp ships its own async-trait, timestamps come from `OffsetDateTime`),
  the deps become permanent dead weight.
- **Suggestion:** Remove both now; Task 18 adds them back with the
  consumer site in the same commit. If Task 18 is in the same PR series,
  a one-line comment in the manifest noting intent is enough.
- **Effort:** XS

### 8. `#[allow(dead_code)]` annotations need a Task 18 audit

- **Severity:** Low
- **Location:** `crates/ohara-mcp/src/server.rs:9, 45`,
  `crates/ohara-mcp/src/tools/find_pattern.rs:5, 10`
- **What:** Four `#[allow(dead_code)]` markers are correct for the
  Task 17 skeleton state but become tech debt the moment Task 18 wires
  the consumers — a forgotten `allow` masks future genuine dead fields.
- **Why:** Not actionable now (load-bearing for clippy-clean status);
  hand-off note for Task 18.
- **Suggestion:** When Task 18 lands the rmcp wiring and the
  `find_pattern` handler, remove all four annotations and let the
  compiler confirm every field is consumed.
- **Effort:** XS (when Task 18 triggers)

---

### See also

- `cargo clippy -p ohara-mcp --all-targets` — clean at HEAD `38268ce`
  (only inherited `ohara-core` warnings remain).
- Inherited from Task 15 (compound here): #1 (panic → `Result`) recurs
  at `server.rs:26`, fix via item #2 above. #3 (`spawn_blocking` `.await??`
  ergonomics) recurs at `server.rs:33`. #5 (helper-level tests) — three
  call sites now.
- Inherited from Task 16: #4 (count-only revwalk) and #5 (extract
  `commits_behind`) — Task 17 is the trigger; item #1 above is the
  concrete follow-through.
- Plan-aware: Task 18 replaces `tools::serve` and `OharaService` stubs
  (not a refactor item) and should resolve item #8. Task 20 e2e will
  exercise `OharaServer::open` and surface item #3 under realistic load.
- Time-sensitive: items #1 + #2 before Task 18 lands (cheaper to
  extract before the second consumer grows code that reaches in).
  Items #3, #5, #6 anytime. Items #4, #7, #8 are Plan 2 / Task 18
  housekeeping.
