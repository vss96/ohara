# ohara v0.5 — `explain_change` MCP tool implementation plan

> **For agentic workers:** TDD red/green commits per task. Standards
> match Plan 1–4 (no commit attribution; workspace-green at every
> commit; cargo fmt + clippy + test clean at end).

**Goal:** ship the `explain_change` MCP tool — given a file + line
range, return the commits that introduced and shaped that code.
Deterministic (git blame, no embeddings), exact provenance, ordered
newest-first.

**Architecture:** new `ohara-git::Blamer` (wrapping `git2::blame_file`)
+ new `Storage::get_commit` and `Storage::get_hunks_for_file_in_commit`
+ new `ohara-core::explain` orchestrator + new MCP tool + new CLI
subcommand. No retriever / migration / parse changes.

**Tech Stack:** Rust 2021, git2 (already a dep), tree-sitter unchanged,
rmcp 0.1.5 unchanged. All else inherited from Plan 4.

---

## 1. Interface contracts

### 1.1 `Storage` additions (lives in `crates/ohara-core/src/storage.rs`)

```rust
async fn get_commit(
    &self,
    repo_id: &RepoId,
    sha: &str,
) -> Result<Option<CommitMeta>>;

async fn get_hunks_for_file_in_commit(
    &self,
    repo_id: &RepoId,
    sha: &str,
    file_path: &str,
) -> Result<Vec<Hunk>>;
```

### 1.2 `BlameSource` trait (lives in `crates/ohara-core/src/explain.rs`)

```rust
#[async_trait]
pub trait BlameSource: Send + Sync {
    async fn blame_range(
        &self, file: &str, line_start: u32, line_end: u32,
    ) -> Result<Vec<BlameRange>>;
}

pub struct BlameRange {
    pub commit_sha: String,
    pub lines: Vec<u32>,
}
```

### 1.3 `explain_change` orchestrator (lives in `ohara-core::explain`)

```rust
pub struct ExplainQuery {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub k: u8,
    pub include_diff: bool,
}

pub struct ExplainHit { /* per spec §"Tool surface" */ }
pub struct ExplainMeta { /* per spec */ }

pub async fn explain_change(
    storage: &dyn Storage,
    blamer: &dyn BlameSource,
    repo_id: &RepoId,
    query: &ExplainQuery,
) -> Result<(Vec<ExplainHit>, ExplainMeta)>;
```

### 1.4 `ohara-git::Blamer`

```rust
pub struct Blamer { repo: Arc<Mutex<Repository>> }

impl Blamer {
    pub fn open(path: &Path) -> Result<Self>;
}

#[async_trait]
impl ohara_core::explain::BlameSource for ohara_git::Blamer {
    async fn blame_range(...) -> Result<Vec<BlameRange>>;
}
```

## 2. File ownership (single track; small scope)

| File | Status |
|------|--------|
| `crates/ohara-core/src/storage.rs` | edit (trait additions) |
| `crates/ohara-core/src/explain.rs` | new |
| `crates/ohara-core/src/lib.rs` | edit (re-export `explain_change`, `ExplainQuery`, `ExplainHit`, `ExplainMeta`, `BlameRange`, `BlameSource`) |
| `crates/ohara-core/src/query.rs` | edit (FakeStorage stubs for new methods) |
| `crates/ohara-core/src/retriever.rs` | edit (FakeStorage stubs) |
| `crates/ohara-storage/src/commit.rs` | edit (add `get` fn) |
| `crates/ohara-storage/src/explain.rs` | new (`get_hunks_for_file_in_commit`) |
| `crates/ohara-storage/src/storage_impl.rs` | edit (wire new methods) |
| `crates/ohara-storage/src/lib.rs` | edit (re-export new module) |
| `crates/ohara-git/src/blame.rs` | new |
| `crates/ohara-git/src/lib.rs` | edit (re-export `Blamer`) |
| `crates/ohara-mcp/src/tools/explain_change.rs` | new |
| `crates/ohara-mcp/src/tools/mod.rs` | edit (register tool) |
| `crates/ohara-mcp/src/server.rs` | edit (construct Blamer, wire into OharaService) |
| `crates/ohara-cli/src/commands/explain.rs` | new |
| `crates/ohara-cli/src/commands/mod.rs` | edit (export module) |
| `crates/ohara-cli/src/main.rs` | edit (subcommand) |

No migration, no schema change.

## 3. Ordered tasks (TDD red/green)

### Task 1: Storage trait additions + stubs

- [ ] **1.r:** Add `get_commit` and `get_hunks_for_file_in_commit` to
      `Storage` trait. Add `unreachable!()` stubs to both `FakeStorage`
      fixtures (`query.rs::tests`, `retriever.rs::tests`). Add stub
      `unimplemented!()` impls to `SqliteStorage`. Workspace builds
      green; no test added yet.
      Single commit (manifest-style — call it red because the impls
      are stubs that would panic if hit).

### Task 2: `Storage::get_commit` impl

- [ ] **2.r:** Add unit test in `storage_impl.rs::tests` that asserts
      `get_commit` returns `None` for an unindexed SHA. Test fails
      because impl is `unimplemented!()`.
- [ ] **2.g:** Implement in `crates/ohara-storage/src/commit.rs::get`,
      wire through `storage_impl.rs`. SELECT from `commit_record` JOIN
      derived per-row to `CommitMeta`. Test passes.

### Task 3: `Storage::get_hunks_for_file_in_commit` impl

- [ ] **3.r:** Add unit tests `get_hunks_for_file_in_commit_filters_by_path`
      and `get_hunks_for_file_in_commit_returns_empty_for_unknown_sha`.
      Tests fail.
- [ ] **3.g:** Implement in new `crates/ohara-storage/src/explain.rs`.
      JOIN `hunk` on `file_path_id → file_path` filtered by sha + path.
      Tests pass.

### Task 4: `BlameSource` trait + `BlameRange` type

- [ ] **4.r:** Define types and trait in
      `crates/ohara-core/src/explain.rs`. Add doc-test with a
      `FakeBlamer` that satisfies the contract. Workspace builds.
- [ ] **4.g:** Single commit (no separate red — trait surface only).

### Task 5: `ohara-git::Blamer`

- [ ] **5.r:** Add `crates/ohara-git/src/blame.rs` with `Blamer::open`
      returning a real Repository handle, and a stubbed `blame_range`
      that returns `Err(...)`. Add `blame_range_returns_one_commit_for_single_author_lines`
      using `tempfile` + `git2` to create a 1-commit repo. Test fails.
- [ ] **5.g:** Implement `blame_range` using `repo.blame_file(...)`.
      For each line in `start..=end`, call `blame.get_line(line)`,
      collect into `HashMap<String, Vec<u32>>` (sha → lines), convert
      to `Vec<BlameRange>`. Wrap in `tokio::task::spawn_blocking`
      since git2 is sync. Test passes.

### Task 6: Blamer multi-author + clamp tests

- [ ] **6.r:** `blame_range_returns_distinct_commits_for_multi_author_range`
      and `blame_range_clamps_to_file_length`. Tests fail (or pass —
      depending on edge case handling in 5.g; iterate as needed).
- [ ] **6.g:** Add line-length clamp to `Blamer::blame_range` (if
      not done in 5.g). Tests pass.

### Task 7: `explain_change` orchestrator

- [ ] **7.r:** Add unit tests in `crates/ohara-core/src/explain.rs`:
      `explain_returns_unique_commits_in_recency_order`,
      `explain_clamps_line_range_to_file_bounds`,
      `explain_skips_unindexed_commits_and_notes_in_meta`,
      `explain_blame_coverage_lt_one_when_some_lines_unattributed`,
      `explain_returns_provenance_exact`. Use `FakeStorage` +
      `FakeBlamer`. Tests fail.
- [ ] **7.g:** Implement `explain_change` per spec §Architecture.
      Sort by `commit.ts` desc; cap at `query.k`; populate
      `ExplainMeta` with `lines_queried`, `commits_unique`,
      `blame_coverage`, `limitation`. Tests pass.

### Task 8: MCP tool `explain_change`

- [ ] **8.r:** Create `crates/ohara-mcp/src/tools/explain_change.rs`
      with `ExplainChangeInput { file, line_start, line_end, k,
      include_diff }`, default-fn helpers, doc-test for
      input default values. Tests fail until input struct lands.
- [ ] **8.g:** Wire `explain_change` into `OharaService` (existing
      service struct gets a second `#[tool]` method). The handler:
      - Construct `ExplainQuery` from input
      - Call `ohara_core::explain::explain_change(storage, blamer, repo_id, q)`
      - Wrap result + meta into JSON
      Append `index_status_meta()` like `find_pattern` does.
      Test passes. Add `OharaServer.blamer` field (Arc<Blamer>),
      construct in `OharaServer::open`.

### Task 9: CLI `ohara explain`

- [ ] **9.r:** Add `crates/ohara-cli/src/commands/explain.rs`,
      register in `mod.rs` + `main.rs`. Add red e2e test
      `explain_e2e_returns_retry_commit_for_retry_lines` against
      `fixtures/tiny/repo`. `#[ignore]`'d (model download for embed
      is reused, but explain itself doesn't need embeddings — confirm
      whether the storage open path triggers fastembed). If it does,
      keep the test ignored; if not, demote to a regular test.
      Test fails because the command isn't implemented.
- [ ] **9.g:** Implement `commands::explain::run`. Output JSON
      (matches MCP body shape). Test passes.

### Task 10: Final pass

- [ ] `cargo fmt --all && cargo clippy --workspace --all-targets --
      -D warnings && cargo test --workspace`. Update README to add
      `explain_change` MCP tool to the supported-tools section
      (currently just lists `find_pattern`).

## 4. Done when

- All 10 tasks complete; final pass green.
- `cargo test --workspace` includes ≥ 13 new unit/integration tests.
- A real `.rs` file in `fixtures/tiny/repo` produces correct
  `explain` output via the CLI subcommand.
- README mentions both `find_pattern` and `explain_change` tools.

## 5. Risk / fallback

- **Blame performance.** `git2::blame_file` walks the file's full
  history; on huge files (5k+ lines) this can take seconds. v0.5
  ships without optimization; if profiling shows it bites, v0.5.1
  adds a `Blamer::blame_range_with_cache` or chunks the work.
- **Unindexed commits.** Blame can return a SHA that the local
  ohara index doesn't know about (e.g., commit older than first
  index run). The orchestrator skips those gracefully with a
  log + `commits_unique` adjustment. Documented in spec edge case 3.
- **File renamed.** Default git2 blame stops at the rename boundary.
  v0.5 documents this in `_meta.limitation` and doesn't follow.
  v0.6 may add follow-rename support.
- **MCP tool registration.** rmcp 0.1.5's `#[tool(tool_box)]` macro
  needs both methods on the same `impl` block. Verify during Task 8
  that adding a second tool method doesn't require structural change
  to `OharaService`.

## Files touched (consolidated)

See File Ownership table above — 16 files total, 4 new.
