# Plan 33: Shared Worktree Store (one index for a clone and all its worktrees) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo conventions: commit after each red test and again after each green implementation.

**Goal:** A linked git worktree resolves to its main clone's `RepoId` and `index.sqlite`, sees only commits reachable from its own HEAD, and gets its new commits indexed by the daemon without re-embedding anything another worktree already indexed.

**Architecture:** `ohara-git` resolves a path to a `RepoIdentity` through `commondir`, so every worktree shares one store. A V7 `indexed_tip` table replaces the single watermark with one tip per worktree; the walker hides every tip. A `CommitVisibility` trait (core) with a git implementation filters retrieval candidates to the querying worktree's ancestry before the rerank pool is cut. `ohara-engine` splits its handle into a per-repo store and a per-worktree handle, and the daemon gains three IPC methods that make it the index writer for incremental work.

**Tech Stack:** Rust (tokio, git2 0.18, rusqlite/refinery, lru, tokio-util). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-20-ohara-harness-design.md`, §3 "One store for all worktrees", §12, §13 phase 1a.

**Flagged before you start**

- **The daemon writes the index (spec open question 5, undecided).** This plan implements the spec's stated default: `ohara index --incremental` routes to a daemon that is *already running*, and falls back to today's in-process path otherwise. Tasks 12–14 are the only ones that depend on that answer; tasks 1–11 stand without it.
- **`--rebuild` refusing while runs are active** is not in this plan. There is no run table until phase 1b.
- **Deviations from the spec text, each forced by the code** (details in "Verified context"):
  the visibility filter does not over-fetch from the lanes; `CommitVisibility` is async and batched; `Storage`'s signatures are not changed (a new `TipStore` trait carries the per-worktree state); the `explain_change` fallback uses a new `CommitLookup` trait, not `CommitSource`.

**Verified context (read before deviating)**

- `RepoId::from_parts` (`crates/ohara-core/src/types.rs:11`) is called with `canonicalize(<path argument>)`, not the discovered repo root: `crates/ohara-cli/src/commands/mod.rs:23-33` (`resolve_repo_id`), `crates/ohara-engine/src/engine.rs:158-167` (`open_repo`) and `:279-286` (`invalidate_repo`). A plain clone's id must stay byte-identical, so this plan keeps hashing exactly that string for non-worktree paths.
- `GitWalker::first_commit_sha` (`crates/ohara-git/src/walker.rs:21`) walks first parents from the *current* HEAD. A linked worktree on an orphan branch would get a different root, so for linked worktrees the root is taken from the main worktree's HEAD.
- The watermark is `repo.last_indexed_commit` (`crates/ohara-storage/src/tables/repo.rs:16-39`); the walker does `push_head()` + `hide(since)` (`walker.rs:50`, `:79-85`); the coordinator reads it at `crates/ohara-core/src/indexer/coordinator/mod.rs:194-206`; `Indexer::run` writes it at `crates/ohara-core/src/indexer.rs:299-301` and *always* extracts HEAD symbols (`:291-296`).
- There is **no `CommitSha` newtype** in the workspace; SHAs are `String`/`&str`. This plan adds `CommitSha` and uses it on new APIs only.
- `RankingWeights::lane_top_k` is a `u8` defaulting to 100 and `rerank_top_k` is 20 (`crates/ohara-core/src/retriever/weights.rs`). The lanes already return five times the rerank pool, and 3× of 100 does not fit a `u8`. The filter therefore runs between lane gathering and `ranking::fuse_to_pool` in `crates/ohara-core/src/retriever/coordinator.rs:34-80`; no lane sizes change.
- `retriever::coordinator::run` already has 7 parameters; an eighth trips `clippy::too_many_arguments`. Task 6 bundles the per-query ones.
- `RepoHandle` (`crates/ohara-engine/src/handle.rs`) holds one `repo_path` and one `Blamer` and is cached by `RepoId` (`engine.rs:51`). `MetaCache` is keyed by `RepoId` (`cache/meta.rs:30`); `BlameCache` by `(RepoId, file, HEAD blob oid)` (`cache/blame.rs:9-13`). Both give wrong answers once two worktrees share an id.
- `hydrate_blame_results` (`crates/ohara-core/src/explain/hydrator.rs:79`) skips blame SHAs missing from storage and reports them in `limitation`. `CommitSource` (`indexer.rs:27-42`) has no single-commit lookup.
- Non-test line counts are near the 500 cap in two files this work touches: `crates/ohara-storage/src/storage_impl.rs` (476) and `crates/ohara-cli/src/commands/index.rs` (471). New code goes in new files; neither file may grow by more than a few lines.
- The daemon cannot index today: all indexer wiring lives in `crates/ohara-cli/src/commands/index.rs:320-405`. `ohara-engine` already depends on `ohara-git`, `ohara-parse` and `ohara-embed`, so it can build an `Indexer`. The daemon's embedder is `LazyFastEmbedProvider` (CPU); the post-commit hook already pins `--embed-provider cpu` (`commands/init.rs:27`).
- Conventions for every task: no `else` (guard clauses / `match` / let-else), no `unwrap()`/`expect()` outside tests, files under 500 non-test lines, `thiserror` in libraries, all SQL in `ohara-storage`, `///` docs on new public items, `tracing` not `println!` outside `ohara-cli`. Run `cargo fmt --all` before every commit. No attribution footers in commit messages.

## Global Constraints

- Plain-clone `RepoId` values are byte-identical before and after this plan.
- Migrations are append-only: add `V7__indexed_tip.sql`, edit no existing migration.
- `repo.last_indexed_commit` stays in the schema; after V7 nothing reads it.
- `commit_record` and `hunk` schemas do not change.
- The `Storage` trait's existing method signatures do not change (dozens of test fakes implement it).
- `IndexStatus` and `ExplainHit` JSON shapes do not change (MCP envelope-parity tests in `crates/ohara-mcp/tests/envelope_parity.rs`).
- The head-snapshot symbol lane keeps describing the main worktree's HEAD; indexing a linked worktree never touches `symbol` rows.
- Agents and `ohara-mcp` never write the index.

## Review Focus

1. **A worktree whose branch was deleted and garbage-collected** leaves an `indexed_tip` row pointing at a missing object. Indexing and status must skip it, not fail. (Task 3 test.)
2. **`ohara index` run from a subdirectory of a plain clone** must produce the same `RepoId` it does today. (Task 2 test.)
3. **A bare repository with linked worktrees** has no main working directory. Identity must still resolve, to the bare repo's path. (Task 2 test.)
4. **A query from a detached-HEAD worktree** (the harness's slots detach on release) must work; visibility is keyed on the HEAD commit, never on a branch name. (Task 5 test.)
5. **An index built with `--embed-cache=diff` or an incompatible embedder** must not be extended by the daemon with mismatched vectors; daemon indexing refuses unless compatibility is `Compatible`. (Task 11 test.)

## File Structure

| File | Responsibility |
|---|---|
| `crates/ohara-core/src/worktree.rs` (new) | `CommitSha`, `WorktreeKey`, `IndexedTip`, `TipStore` |
| `crates/ohara-core/src/visibility.rs` (new) | `CommitVisibility`, `VisibilityScope` |
| `crates/ohara-storage/migrations/V7__indexed_tip.sql` (new) | table + backfill |
| `crates/ohara-storage/src/tables/indexed_tip.rs`, `tables/prune.rs` (new) | SQL |
| `crates/ohara-storage/src/tip_store.rs` (new) | `impl TipStore for SqliteStorage` |
| `crates/ohara-git/src/identity.rs` (new) | `RepoIdentity::resolve` |
| `crates/ohara-git/src/visibility.rs` (new) | `GitVisibility`, `reachable_commit_shas` |
| `crates/ohara-engine/src/cache/ancestry.rs` (new) | `(head, sha)` LRU |
| `crates/ohara-engine/src/index_service.rs` (new) | daemon-side indexing, queue, prune |
| `crates/ohara-cli/src/commands/index/daemon_route.rs` (new) | incremental-via-daemon |

---

### Task 1: Core newtypes and the `TipStore` trait (ohara-core)

**Files:**
- Create: `crates/ohara-core/src/worktree.rs`
- Modify: `crates/ohara-core/src/lib.rs` (add `pub mod worktree;` and `pub use worktree::{CommitSha, IndexedTip, TipStore, WorktreeKey};`)

**Interfaces — Produces:**

```rust
pub struct CommitSha(String);          // Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize
impl CommitSha {
    pub fn parse(hex: &str) -> Result<Self>;   // 40 lowercase hex chars, else OhraError::InvalidArgument
    pub fn as_str(&self) -> &str;
}
pub struct WorktreeKey(String);        // same derives
impl WorktreeKey {
    pub fn main() -> Self;                     // ""
    pub fn linked(name: &str) -> Result<Self>; // non-empty, else OhraError::InvalidArgument
    pub fn is_main(&self) -> bool;
    pub fn as_str(&self) -> &str;
}
pub struct IndexedTip { pub worktree: WorktreeKey, pub sha: CommitSha }

#[async_trait]
pub trait TipStore: Send + Sync {
    async fn indexed_tips(&self, repo_id: &RepoId) -> Result<Vec<IndexedTip>>;
    async fn set_indexed_tip(&self, repo_id: &RepoId, worktree: &WorktreeKey, sha: &CommitSha) -> Result<()>;
    async fn remove_indexed_tip(&self, repo_id: &RepoId, worktree: &WorktreeKey) -> Result<()>;
    async fn indexed_commit_shas(&self, repo_id: &RepoId) -> Result<Vec<CommitSha>>;
    /// Deletes each commit with its hunks, vectors and FTS rows. Returns commits deleted.
    async fn delete_commits(&self, repo_id: &RepoId, shas: &[CommitSha]) -> Result<u64>;
}
```

- [ ] **Step 1: Write the failing tests** in `worktree.rs` (`#[cfg(test)] mod tests`):

```rust
#[test]
fn commit_sha_accepts_40_hex_and_rejects_everything_else() {
    assert!(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").is_ok());
    assert!(CommitSha::parse("abc").is_err());
    assert!(CommitSha::parse("0123456789ABCDEF0123456789abcdef01234567").is_err());
    assert!(CommitSha::parse("z123456789abcdef0123456789abcdef01234567").is_err());
}

#[test]
fn worktree_key_main_is_empty_and_linked_rejects_empty() {
    assert_eq!(WorktreeKey::main().as_str(), "");
    assert!(WorktreeKey::main().is_main());
    assert!(WorktreeKey::linked("").is_err());
    let k = WorktreeKey::linked("slot-1").unwrap();
    assert_eq!(k.as_str(), "slot-1");
    assert!(!k.is_main());
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-core worktree::` — Expected: COMPILE ERROR (`CommitSha` not found).
- [ ] **Step 3: Commit the red test** — `git commit -m "test(core): CommitSha and WorktreeKey contracts (red)"`
- [ ] **Step 4: Implement** the types and the trait exactly as in Interfaces. `TipStore` gets `///` docs per method; no default methods.
- [ ] **Step 5: Run** `cargo test -p ohara-core worktree::` — Expected: 2 passed.
- [ ] **Step 6: Commit** — `git commit -m "feat(core): CommitSha, WorktreeKey and TipStore trait"`

**Acceptance:** `cargo test -p ohara-core worktree::`

---

### Task 2: `RepoIdentity` — commondir-aware repo id (ohara-git)

**Files:**
- Create: `crates/ohara-git/src/identity.rs`
- Modify: `crates/ohara-git/src/lib.rs` (`pub mod identity; pub use identity::RepoIdentity;`)

**Interfaces — Consumes:** `RepoId::from_parts`, `WorktreeKey`. **Produces:**

```rust
pub struct RepoIdentity {
    pub repo_id: RepoId,
    /// Canonical path of the worktree the caller is in (what git2 should open).
    pub worktree_path: PathBuf,
    /// The string that was hashed into `repo_id`.
    pub id_path: String,
    pub first_commit_sha: String,
    pub worktree: WorktreeKey,
}
impl RepoIdentity {
    pub fn resolve(path: &Path) -> anyhow::Result<Self>;
}
```

Resolution rules, in order:

1. `canonical = fs::canonicalize(path)`; `repo = Repository::discover(&canonical)`.
2. **Not a linked worktree** (`!repo.is_worktree()`): `id_path = canonical` (the argument, exactly as today), `first_commit_sha = GitWalker::open(&canonical)?.first_commit_sha()`, `worktree = WorktreeKey::main()`, `worktree_path = canonical`.
3. **Linked worktree:** open the main repository with `Repository::open(repo.commondir())`. `id_path` is `canonicalize(main.workdir())`, or `canonicalize(repo.commondir())` when the main repo is bare. `first_commit_sha` comes from the *main* repository's HEAD (first-parent root); if the main HEAD is unborn, fall back to the worktree's HEAD. `worktree = WorktreeKey::linked(<file name of repo.path()>)` (the `.git/worktrees/<name>` directory). `worktree_path = canonicalize(repo.workdir())`.
4. `repo_id = RepoId::from_parts(&first_commit_sha, &id_path)`.

- [ ] **Step 1: Write the failing tests** in `identity.rs`. Shared helper `init_repo_with_commit(dir) -> Repository` (one empty-tree commit on HEAD) and `add_worktree(repo, name, path)` using `repo.worktree(name, path, None)`.

```rust
#[test]
fn plain_clone_id_is_byte_identical_to_legacy_formula() {
    let dir = tempfile::tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let first = GitWalker::open(&canonical).unwrap().first_commit_sha().unwrap();
    let legacy = RepoId::from_parts(&first, &canonical.to_string_lossy());
    let id = RepoIdentity::resolve(dir.path()).unwrap();
    assert_eq!(id.repo_id, legacy);
    assert!(id.worktree.is_main());
}

#[test]
fn subdirectory_argument_keeps_its_legacy_id() {
    // Review Focus 2: today the argument path is hashed, not the repo root.
    let dir = tempfile::tempdir().unwrap();
    init_repo_with_commit(dir.path());
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let canonical = std::fs::canonicalize(&sub).unwrap();
    let first = GitWalker::open(&canonical).unwrap().first_commit_sha().unwrap();
    assert_eq!(
        RepoIdentity::resolve(&sub).unwrap().repo_id,
        RepoId::from_parts(&first, &canonical.to_string_lossy())
    );
}

#[test]
fn linked_worktree_shares_main_id_and_reports_its_name() {
    let main = tempfile::tempdir().unwrap();
    let wt_parent = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(main.path());
    add_worktree(&repo, "slot-1", &wt_parent.path().join("slot-1"));
    let main_id = RepoIdentity::resolve(main.path()).unwrap();
    let wt_id = RepoIdentity::resolve(&wt_parent.path().join("slot-1")).unwrap();
    assert_eq!(wt_id.repo_id, main_id.repo_id);
    assert_eq!(wt_id.worktree.as_str(), "slot-1");
    assert_ne!(wt_id.worktree_path, main_id.worktree_path);
}

#[test]
fn separate_clones_get_distinct_ids() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    init_repo_with_commit(a.path());
    git2::Repository::clone(a.path().to_str().unwrap(), b.path()).unwrap();
    assert_ne!(
        RepoIdentity::resolve(a.path()).unwrap().repo_id,
        RepoIdentity::resolve(b.path()).unwrap().repo_id
    );
}

#[test]
fn worktree_of_bare_repo_resolves_to_the_bare_path() {
    // Review Focus 3.
    let bare = tempfile::tempdir().unwrap();
    let wt_parent = tempfile::tempdir().unwrap();
    let repo = init_bare_repo_with_commit(bare.path());
    add_worktree(&repo, "w", &wt_parent.path().join("w"));
    let id = RepoIdentity::resolve(&wt_parent.path().join("w")).unwrap();
    assert_eq!(id.id_path, std::fs::canonicalize(bare.path()).unwrap().to_string_lossy());
}

#[test]
fn linked_worktree_on_orphan_branch_still_shares_main_id() {
    // Root is taken from the main worktree's HEAD, not the orphan's.
    let main = tempfile::tempdir().unwrap();
    let wt_parent = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(main.path());
    let wt = wt_parent.path().join("orphan");
    add_worktree(&repo, "orphan", &wt);
    commit_orphan_root_on_head(&wt); // new parentless commit, HEAD moved to it
    assert_eq!(
        RepoIdentity::resolve(&wt).unwrap().repo_id,
        RepoIdentity::resolve(main.path()).unwrap().repo_id
    );
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-git identity::` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red test** — `git commit -m "test(git): RepoIdentity resolves worktrees to the main clone (red)"`
- [ ] **Step 4: Implement `RepoIdentity::resolve`** per the rules above. Add `GitWalker::from_repository(repo: Repository) -> Self` if needed to reuse `first_commit_sha` on the main repo.
- [ ] **Step 5: Run** `cargo test -p ohara-git identity::` — Expected: 6 passed.
- [ ] **Step 6: Commit** — `git commit -m "feat(git): commondir-aware RepoIdentity"`

**Acceptance:** `cargo test -p ohara-git identity::`

---

### Task 3: V7 `indexed_tip` migration and `TipStore` for SQLite (ohara-storage)

**Files:**
- Create: `crates/ohara-storage/migrations/V7__indexed_tip.sql`
- Create: `crates/ohara-storage/src/tables/indexed_tip.rs`
- Create: `crates/ohara-storage/src/tip_store.rs`
- Modify: `crates/ohara-storage/src/tables/mod.rs`, `crates/ohara-storage/src/lib.rs` (module declarations only)
- Modify: `crates/ohara-storage/src/tables/repo.rs:16-39` (`get_status`, `set_watermark`)
- Test: `crates/ohara-storage/src/migrations.rs` (tests module), `crates/ohara-storage/src/tip_store.rs` (tests module)

**Interfaces — Consumes:** `TipStore`, `IndexedTip`, `WorktreeKey`, `CommitSha` (Task 1). **Produces:** `impl TipStore for SqliteStorage` (the three tip methods here; the two prune methods return `Ok(vec![])` / `Ok(0)` until Task 11, each with a `// Task 11` comment). `Storage::get_index_status` and `Storage::set_last_indexed_commit` keep their signatures and now read/write the `''` (main) tip.

Migration, verbatim:

```sql
-- Plan 33: one indexed tip per worktree replaces the single
-- repo.last_indexed_commit watermark. '' is the main worktree.
-- repo.last_indexed_commit is kept but no longer read.
CREATE TABLE indexed_tip (
    repo_id      TEXT NOT NULL,
    worktree_key TEXT NOT NULL,
    sha          TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (repo_id, worktree_key)
);
INSERT INTO indexed_tip (repo_id, worktree_key, sha, updated_at)
    SELECT id, '', last_indexed_commit, COALESCE(indexed_at, '')
    FROM repo WHERE last_indexed_commit IS NOT NULL;
```

- [ ] **Step 1: Write the failing tests.**

In `migrations.rs` tests (run to V6 with `embedded::migrations::runner().set_target(refinery::Target::Version(6))`, seed, then run all):

```rust
#[test]
fn v7_backfills_indexed_tip_from_legacy_watermark() {
    let mut c = fresh_conn();                      // same setup as the existing tests
    run_to_version(&mut c, 6);
    c.execute(
        "INSERT INTO repo (id, path, first_commit_sha, last_indexed_commit, indexed_at, schema_version)
         VALUES ('r1', '/p', 'f', 'aaaa', '2026-01-01T00:00:00Z', 1),
                ('r2', '/q', 'g', NULL, NULL, 1)",
        [],
    ).unwrap();
    run(&mut c).unwrap();
    let rows: Vec<(String, String, String)> = query_all(
        &c, "SELECT repo_id, worktree_key, sha FROM indexed_tip ORDER BY repo_id");
    assert_eq!(rows, vec![("r1".into(), "".into(), "aaaa".into())]);
}
```

In `tip_store.rs` tests (`SqliteStorage::open` on a tempfile, `open_repo` first):

```rust
#[tokio::test]
async fn set_and_list_tips_per_worktree() {
    // set main -> A, slot-1 -> B, then slot-1 -> C (upsert)
    // indexed_tips == [("", A), ("slot-1", C)] ordered by worktree_key
}

#[tokio::test]
async fn remove_indexed_tip_deletes_only_that_worktree() { /* main survives */ }

#[tokio::test]
async fn legacy_watermark_methods_read_and_write_the_main_tip() {
    // storage.set_last_indexed_commit(&id, A) -> indexed_tips == [("", A)]
    // tips.set_indexed_tip(&id, &WorktreeKey::main(), B)
    //   -> storage.get_index_status(&id).last_indexed_commit == Some(B)
    //   and .indexed_at is Some
    // a linked tip never changes get_index_status
}

#[tokio::test]
async fn tips_route_through_the_single_writer_pool() {
    // 16 concurrent set_indexed_tip calls all succeed (mirrors the plan-32 test)
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-storage -- v7_ tip` — Expected: COMPILE ERROR / missing table.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(storage): V7 indexed_tip backfill and TipStore contract (red)"`
- [ ] **Step 4: Implement.**
  - `tables/indexed_tip.rs`: `list(c, repo_id) -> Result<Vec<(String, String)>>` ordered by `worktree_key`; `upsert(c, repo_id, key, sha)` (`INSERT … ON CONFLICT(repo_id, worktree_key) DO UPDATE`, `updated_at` = RFC 3339 now); `remove(c, repo_id, key)`.
  - `tables/repo.rs`: `get_status` selects the `''` tip from `indexed_tip` joined with `repo.indexed_at`; `set_watermark` upserts the `''` tip and updates `repo.indexed_at` in one transaction. It no longer writes `repo.last_indexed_commit`.
  - `tip_store.rs`: `impl TipStore for SqliteStorage`. Reads use `self.pool()`, writes use `self.write_pool()`, both through the existing `with_conn` helper in `storage_impl.rs` (make it `pub(crate)` if it is not). `set_indexed_tip` with a main key also updates `repo.indexed_at` (call `repo::set_watermark`). Rows whose `sha` fails `CommitSha::parse` are skipped with a `tracing::warn!`.
  - Do not add methods to `storage_impl.rs`.
- [ ] **Step 5: Run** `cargo test -p ohara-storage` — Expected: all pass, including the pre-existing watermark tests.
- [ ] **Step 6: Commit** — `git commit -m "feat(storage): V7 indexed_tip table and SqliteStorage TipStore"`

**Acceptance:** `cargo test -p ohara-storage -- v7_ tip`

---

### Task 4: Multi-tip commit walk (ohara-git, ohara-core)

**Files:**
- Modify: `crates/ohara-git/src/walker.rs:77-107` (`list_commits`)
- Modify: `crates/ohara-git/src/lib.rs:68-86` (`GitCommitSource::list_commits`), `:176-193` (`GitCommitsBehind`)
- Modify: `crates/ohara-core/src/indexer.rs:27-42` (`CommitSource`), `crates/ohara-core/src/query.rs:103-108` (`CommitsBehind`)

**Interfaces — Produces:**

```rust
// ohara-git
impl GitWalker {
    /// Commits reachable from HEAD and from none of `hide`, oldest first.
    /// A hide entry that is not a valid oid or whose object is missing is skipped.
    pub fn list_commits_hiding(&self, hide: &[&str]) -> Result<Vec<CommitMeta>>;
}
// ohara-core — default methods, so existing fakes keep compiling
trait CommitSource {
    async fn list_commits_hiding(&self, hide: &[String]) -> Result<Vec<CommitMeta>> {
        self.list_commits(hide.first().map(String::as_str)).await
    }
}
trait CommitsBehind {
    async fn count_hiding(&self, hide: &[String]) -> Result<u64> {
        self.count_since(hide.first().map(String::as_str)).await
    }
}
```

`GitWalker::list_commits(since)` becomes a one-line call to `list_commits_hiding`. `GitCommitSource` and `GitCommitsBehind` override the new methods with real multi-hide walks.

- [ ] **Step 1: Write the failing tests** in `walker.rs` tests. Fixture: linear `c1 → c2 → c3` on main, then branch `feat` from `c2` with `f1`; HEAD on `feat`.

```rust
#[test]
fn hiding_several_tips_returns_only_commits_none_of_them_reach() {
    // HEAD = f1. hide [c3] -> [f1]   (c1, c2 are ancestors of c3)
    // hide [c2, c3] -> [f1]
    // hide [] -> [c1, c2, f1]
}

#[test]
fn missing_or_malformed_hide_entries_are_skipped() {
    // Review Focus 1.
    // hide ["0123456789abcdef0123456789abcdef01234567" /* no such object */, "not-a-sha", c2]
    //   -> [f1]   and no error
}

#[test]
fn list_commits_since_is_unchanged() {
    // HEAD = main (c3). list_commits(Some(c1)) == [c2, c3]; list_commits(None) == [c1, c2, c3]
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-git walker::` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(git): multi-tip hide walk skips missing tips (red)"`
- [ ] **Step 4: Implement** as in Interfaces. Extract the `CommitMeta` construction duplicated at `walker.rs:57-69` and `:92-104` into one private `fn commit_meta(c: &git2::Commit) -> CommitMeta` (later tasks reuse it).
- [ ] **Step 5: Run** `cargo test -p ohara-git && cargo test -p ohara-core` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(git): walk from HEAD hiding every indexed tip"`

**Acceptance:** `cargo test -p ohara-git walker::`

---

### Task 5: Indexer consumes tips (ohara-core)

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs` (`Indexer` struct ~`:118`, builder ~`:215-232`, `run` `:239-310`)
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs:185-206` (stage 0/1)
- Modify: `crates/ohara-core/src/indexer/stages/commit_walk.rs`
- Test: `crates/ohara-core/src/indexer/coordinator/tests.rs`, `crates/ohara-core/src/indexer.rs` tests

**Interfaces — Consumes:** `TipStore`, `IndexedTip`, `WorktreeKey`, `CommitSha`, `CommitSource::list_commits_hiding`. **Produces:**

```rust
pub struct TipContext {
    pub tips: Arc<dyn TipStore>,
    pub worktree: WorktreeKey,
    pub head: CommitSha,      // the worktree's HEAD when the run started
}
impl Indexer     { pub fn with_tip_context(self, context: TipContext) -> Self; }
impl Coordinator { pub fn with_hidden_tips(self, hide: Vec<CommitSha>) -> Self; }
impl CommitWalkStage {
    pub async fn run_hiding(source: &dyn CommitSource, hide: &[CommitSha]) -> Result<Vec<CommitMeta>>;
}
```

Behaviour with a `TipContext`:

1. `Indexer::run` reads `tips.indexed_tips(repo_id)` and passes every sha to `Coordinator::with_hidden_tips`. The coordinator then calls `CommitWalkStage::run_hiding` instead of reading `get_index_status`.
2. On success (`commits_failed == 0`) it calls `tips.set_indexed_tip(repo_id, &worktree, &head)`, even when the walk was empty, and does **not** call `Storage::set_last_indexed_commit`.
3. When `worktree` is not main it skips HEAD-symbol extraction and `put_head_symbols` entirely (`IndexerReport::head_symbols == 0`).

Without a `TipContext` the behaviour is exactly today's.

- [ ] **Step 1: Write the failing tests.** Add an in-memory `FakeTips` (`Mutex<Vec<IndexedTip>>`) beside the existing fakes, and a `HidingSource` fake whose `list_commits_hiding` records its argument.

```rust
#[tokio::test]
async fn indexer_hides_every_indexed_tip_and_records_the_worktree_head() {
    // tips preloaded: ("", A), ("slot-2", B). TipContext { worktree: linked("slot-1"), head: H }
    // -> source saw hide == [A, B] (any order)
    // -> tips now contains ("slot-1", H); ("", A) unchanged
    // -> storage.set_last_indexed_commit was never called
}

#[tokio::test]
async fn linked_worktree_run_never_touches_head_symbols() {
    // symbol source panics if extract_head_symbols is called; run succeeds;
    // storage.put_head_symbols call count == 0; report.head_symbols == 0
}

#[tokio::test]
async fn empty_walk_still_records_the_tip() {
    // source returns no commits; tips contains ("slot-1", H) afterwards
}

#[tokio::test]
async fn failed_commits_leave_the_tip_unset() {
    // source whose hunks_for_commit errors for one sha -> commits_failed == 1; no ("slot-1", _) tip
}

#[tokio::test]
async fn without_tip_context_the_legacy_watermark_path_is_unchanged() {
    // existing behaviour: get_index_status read, set_last_indexed_commit written, head symbols extracted
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-core indexer` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(core): indexer walks from HEAD hiding all tips (red)"`
- [ ] **Step 4: Implement** per Behaviour. `indexer.rs` is at 333 non-test lines; keep `TipContext` and its two helper methods (`hidden`, `record`) in a new `crates/ohara-core/src/indexer/tip_context.rs` if `run` would otherwise pass 50 lines.
- [ ] **Step 5: Run** `cargo test -p ohara-core` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(core): per-worktree tips in the indexer"`

**Acceptance:** `cargo test -p ohara-core indexer`

---

### Task 6: `CommitVisibility` trait and the retriever post-filter (ohara-core)

**Files:**
- Create: `crates/ohara-core/src/visibility.rs`
- Modify: `crates/ohara-core/src/lib.rs` (module + re-exports)
- Modify: `crates/ohara-core/src/retriever/coordinator.rs:34-80` and its tests
- Modify: `crates/ohara-core/src/retriever/mod.rs:75-130` (`find_pattern`, `find_pattern_with_profile`)
- Test: `crates/ohara-core/src/retriever/tests/pipeline.rs`

**Interfaces — Produces:**

```rust
// ohara-core::visibility
#[async_trait]
pub trait CommitVisibility: Send + Sync {
    /// One bool per entry of `commits`: true when it is `head` or an ancestor of `head`.
    /// A commit whose object is missing is not visible.
    async fn visible(&self, head: &CommitSha, commits: &[CommitSha]) -> Result<Vec<bool>>;
}
pub struct VisibilityScope<'a> {
    pub head: &'a CommitSha,
    pub oracle: &'a dyn CommitVisibility,
}

// retriever::coordinator — replaces the last four positional parameters of `run`
pub struct RunContext<'a> {
    pub query: &'a PatternQuery,
    pub repo_id: &'a RepoId,
    pub final_k: usize,
    pub now_unix: i64,
    pub visibility: Option<VisibilityScope<'a>>,
}
pub async fn run(
    lanes: &[Box<dyn RetrievalLane>],
    weights: &RankingWeights,
    reranker: Option<&Arc<dyn RerankProvider>>,
    context: RunContext<'_>,
) -> crate::Result<Vec<HunkHit>>;

// Retriever
pub async fn find_pattern_scoped(
    &self, repo_id: &RepoId, query: &PatternQuery, now_unix: i64,
    visibility: Option<VisibilityScope<'_>>,
) -> Result<(Vec<PatternHit>, /* same profile type as find_pattern_with_profile */)>;
```

`find_pattern_with_profile` delegates to `find_pattern_scoped(…, None)`.

Filter placement: after the per-lane rankings and `by_id` map are built (`coordinator.rs:55-70`) and before `ranking::fuse_to_pool` (`:74`). Collect the distinct `commit.commit_sha` values in `by_id`, ask the oracle once, then drop invisible ids from every ranking and from `by_id`. A sha that fails `CommitSha::parse` is treated as invisible. With `visibility: None` nothing changes. No lane sizes change: lanes already gather `lane_top_k` (100) against a rerank pool of 20.

- [ ] **Step 1: Write the failing tests** in `coordinator.rs` tests (reuse `make_hit` / the lane fake; give hits distinct `commit.commit_sha` values). `SetOracle(HashSet<String>)` is a fake where a commit is visible iff it is in the set; it also counts calls.

```rust
#[tokio::test]
async fn invisible_commits_are_removed_before_the_rerank_pool_is_cut() {
    // rerank_top_k = 2. Lane returns 4 hits best-first on commits [x1, x2, v1, v2];
    // oracle: only v1, v2 visible. Result commits == [v1, v2].
    // (Filtering after the cut would return nothing.)
}

#[tokio::test]
async fn oracle_is_asked_once_with_distinct_commits() {
    // two lanes, overlapping hits on 3 distinct commits -> 1 oracle call with 3 shas
}

#[tokio::test]
async fn no_scope_means_no_filtering_and_no_oracle_call() { /* visibility: None */ }

#[tokio::test]
async fn all_candidates_invisible_returns_empty() { /* Ok(vec![]) */ }
```

Update the existing coordinator tests' `run(…)` call sites to the `RunContext` form in the same step (they must still fail to compile only because of the new names).

- [ ] **Step 2: Run** `cargo test -p ohara-core retriever` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(core): visibility post-filter ahead of the rerank pool (red)"`
- [ ] **Step 4: Implement** `visibility.rs`, `RunContext`, the filter (a private `async fn retain_visible(rankings, by_id, scope) -> Result<()>` in `coordinator.rs`), and `Retriever::find_pattern_scoped`.
- [ ] **Step 5: Run** `cargo test -p ohara-core` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(core): CommitVisibility and per-worktree retrieval filter"`

**Acceptance:** `cargo test -p ohara-core retriever`

---

### Task 7: `GitVisibility` and reachability (ohara-git)

**Files:**
- Create: `crates/ohara-git/src/visibility.rs`
- Modify: `crates/ohara-git/src/lib.rs` (`pub mod visibility; pub use visibility::{reachable_commit_shas, GitVisibility};`)

**Interfaces — Consumes:** `CommitVisibility`, `CommitSha`. **Produces:**

```rust
pub struct GitVisibility { /* repo path */ }
impl GitVisibility { pub fn open(path: &Path) -> anyhow::Result<Self>; }
impl CommitVisibility for GitVisibility { /* visible(): one spawn_blocking for the whole batch */ }

/// Every commit reachable from any ref (`refs/*`) or from any worktree's HEAD,
/// including detached ones. Used by prune (Task 11).
pub fn reachable_commit_shas(path: &Path) -> anyhow::Result<HashSet<String>>;
```

`visible` uses `head == commit || repo.graph_descendant_of(head, commit)?`; an oid lookup failure for `commit` yields `false`, for `head` an error.

- [ ] **Step 1: Write the failing tests** (fixture from Task 4: `c1 → c2 → c3` on main, `f1` on `feat` from `c2`).

```rust
#[tokio::test]
async fn visible_is_head_or_ancestor() {
    // head = c3: [c1, c2, c3, f1] -> [true, true, true, false]
    // head = f1: [c1, c2, c3, f1] -> [true, true, false, true]
}

#[tokio::test]
async fn detached_head_worktree_is_keyed_on_the_commit() {
    // Review Focus 4: head given as f1's sha with no branch involved -> same answers
}

#[tokio::test]
async fn missing_commit_is_invisible_and_missing_head_is_an_error() {}

#[test]
fn reachable_includes_branches_and_detached_worktree_heads_but_not_dangling_commits() {
    // create dangling commit d (no ref). Linked worktree detached at f1 after deleting branch feat.
    // reachable ⊇ {c1, c2, c3, f1}; d ∉ reachable
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-git visibility::` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(git): GitVisibility ancestry and ref reachability (red)"`
- [ ] **Step 4: Implement.** `reachable_commit_shas`: revwalk with `push_glob("refs/*")`, `push_head()` on the main repo, and for each `repo.worktrees()` entry open it and push its HEAD target (skip worktrees that fail to open).
- [ ] **Step 5: Run** `cargo test -p ohara-git visibility::` — Expected: 4 passed.
- [ ] **Step 6: Commit** — `git commit -m "feat(git): GitVisibility and reachable_commit_shas"`

**Acceptance:** `cargo test -p ohara-git visibility::`

---

### Task 8: Engine handle split and per-worktree caches (ohara-engine)

**Files:**
- Modify: `crates/ohara-engine/src/handle.rs` (whole file)
- Modify: `crates/ohara-engine/src/engine.rs:46-60`, `:155-210` (`open_repo`), `:278-291` (`invalidate_repo`), `:302-360` (`explain_change`), `:370-395` (`compose_response_meta`)
- Modify: `crates/ohara-engine/src/cache/meta.rs`, `crates/ohara-engine/src/cache/blame.rs:9-13`
- Modify: `crates/ohara-mcp/src/tools/find_pattern.rs` (3 `handle.` uses)
- Test: `crates/ohara-engine/src/engine_tests.rs`

**Interfaces — Consumes:** `RepoIdentity` (Task 2), `TipStore` (Task 3), `CommitsBehind::count_hiding` (Task 4). **Produces:**

```rust
/// Shared by every worktree of one repository.
pub struct RepoStore {
    pub repo_id: RepoId,
    pub storage: Arc<dyn Storage>,
    pub tips: Arc<dyn TipStore>,
    pub retriever: Retriever,
}
/// One per worktree path.
pub struct RepoHandle {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,          // this worktree
    pub worktree: WorktreeKey,
    pub store: Arc<RepoStore>,
    pub blamer: Arc<Blamer>,
}
```

- `RetrievalEngine` holds `stores: RwLock<HashMap<RepoId, Arc<RepoStore>>>` and `repos: RwLock<HashMap<PathBuf, Arc<RepoHandle>>>` (keyed by canonical worktree path). `open_repo` resolves `RepoIdentity`, reuses or opens the store, then builds the per-worktree handle. `SqliteStorage` is opened once and the same `Arc` is used as both `storage` and `tips`.
- `invalidate_repo(path)` drops the `RepoStore` and **every** `RepoHandle` with that `repo_id`.
- `MetaCache` key becomes `(RepoId, WorktreeKey)`; `invalidate(&RepoId)` removes all worktrees of the repo. `BlameCache`'s `Key` gains `worktree: WorktreeKey`; `get`/`put` take it as a new second argument.
- `compose_response_meta` computes freshness per worktree: `last_indexed_commit` is this worktree's tip if it has one, else the main tip; `commits_behind_head` is `GitCommitsBehind::open(&handle.repo_path)?.count_hiding(<all tip shas>)`. Put this in `ohara_core::query::compute_worktree_index_status(storage, tips, repo_id, worktree, behind) -> Result<IndexStatus>`; `compute_index_status` stays for callers without tips.

- [ ] **Step 1: Write the failing tests** in `engine_tests.rs`. Helper `repo_with_worktree() -> (TempDir main, PathBuf worktree)`: main clone with two commits indexed by the existing test indexer helper, plus a linked worktree `slot-1` on a new branch.

```rust
#[tokio::test]
async fn worktree_and_main_share_one_store_but_not_one_handle() {
    // Arc::ptr_eq(&main.store, &wt.store); main.repo_id == wt.repo_id
    // main.repo_path != wt.repo_path; wt.worktree.as_str() == "slot-1"
    // blamer paths differ
}

#[tokio::test]
async fn worktree_needs_no_reindex_to_report_fresh() {
    // main indexed to HEAD; worktree HEAD == main HEAD
    // engine.index_status(worktree).index_status.commits_behind_head == 0
}

#[tokio::test]
async fn commits_behind_is_per_worktree() {
    // commit once in the worktree only:
    // index_status(worktree).commits_behind_head == 1; index_status(main) == 0
    // (MetaCache must not serve one worktree's answer to the other)
}

#[tokio::test]
async fn invalidate_repo_drops_every_worktree_handle() {
    // open both; invalidate via the worktree path; reopening main yields a new store (!Arc::ptr_eq)
}

#[tokio::test]
async fn blame_cache_does_not_cross_worktrees() {
    // same file content at both HEADs but the worktree's line 1 was last touched by a
    // worktree-only commit (amend-style rewrite). explain_change(main) then explain_change(worktree):
    // the two results name different commit SHAs.
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-engine` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(engine): one store per repo, one handle per worktree (red)"`
- [ ] **Step 4: Implement** per Interfaces. `engine.rs` is at 107 lines before its test-module attribute but the `impl` block is long; if the file passes 450 non-test lines move `open_repo` + `invalidate_repo` into a new `crates/ohara-engine/src/engine/open.rs` `impl RetrievalEngine` block.
- [ ] **Step 5: Run** `cargo test -p ohara-engine && cargo test -p ohara-mcp` — Expected: all pass (envelope parity unchanged).
- [ ] **Step 6: Commit** — `git commit -m "refactor(engine): split RepoStore from per-worktree RepoHandle"`

**Acceptance:** `cargo test -p ohara-engine -- worktree commits_behind invalidate_repo blame_cache`

---

### Task 9: Visibility wiring and `AncestryCache` (ohara-engine)

**Files:**
- Create: `crates/ohara-engine/src/cache/ancestry.rs`
- Modify: `crates/ohara-engine/src/cache/mod.rs`, `crates/ohara-engine/src/lib.rs` (`pub use cache::AncestryCache;`)
- Modify: `crates/ohara-engine/src/engine.rs` (`find_pattern`, `:234-259`)
- Test: `crates/ohara-engine/src/cache/ancestry.rs`, `crates/ohara-engine/src/engine_tests.rs`

**Interfaces — Consumes:** `CommitVisibility`, `VisibilityScope`, `Retriever::find_pattern_scoped` (Task 6), `GitVisibility` (Task 7), `RepoHandle.store.tips` (Task 8). **Produces:**

```rust
/// LRU of (head, commit) -> visible. Ancestry between two commits never changes,
/// so entries are never invalidated.
pub struct AncestryCache { /* Mutex<LruCache<(CommitSha, CommitSha), bool>> */ }
impl AncestryCache {
    pub fn new(capacity: usize) -> Self;                       // default capacity 65_536
    pub fn hits(&self) -> u64;
}
/// Decorator: answers from the cache, asks `inner` once for the misses.
pub struct CachedVisibility { /* inner: Arc<dyn CommitVisibility>, cache: Arc<AncestryCache> */ }
impl CommitVisibility for CachedVisibility { … }
```

`RetrievalEngine::find_pattern` flow after the `NeedsRebuild` guard:

1. `head` = HEAD sha of `handle.repo_path`.
2. `tips = handle.store.tips.indexed_tips(&repo_id)`.
3. **Fast path:** ask the cached oracle about the tip shas; if all are visible from `head`, call `find_pattern_scoped(…, None)`.
4. Otherwise call it with `Some(VisibilityScope { head: &head, oracle: &cached })`.

One `AncestryCache` per engine, shared by all repos (the key already contains full SHAs).

- [ ] **Step 1: Write the failing tests.**

`ancestry.rs`:

```rust
#[tokio::test]
async fn second_lookup_is_served_from_cache() {
    // CountingOracle inner. visible(h, [a, b]) twice -> inner called once with 2 shas; hits() == 2
}

#[tokio::test]
async fn only_misses_reach_the_inner_oracle() {
    // visible(h, [a]) then visible(h, [a, b]) -> second inner call receives [b] only;
    // returned Vec<bool> is in input order
}
```

`engine_tests.rs` (helper from Task 8; index the worktree's extra commit with an `Indexer` carrying a `TipContext` for `slot-1`, and a second worktree `slot-2` on another branch):

```rust
#[tokio::test]
async fn run_branch_commit_is_visible_only_from_its_own_worktree() {
    // commit W ("add frobnicate retry") made and indexed in slot-1
    // find_pattern(slot-1, "frobnicate retry") hits contain W
    // find_pattern(main,   same)               hits do not contain W
    // find_pattern(slot-2, same)               hits do not contain W
}

#[tokio::test]
async fn filter_is_skipped_when_every_tip_is_visible() {
    // only the main tip exists -> engine.ancestry_filtered_queries() == 0 after a query from main
}
```

Add `pub fn ancestry_filtered_queries(&self) -> u64` (an `AtomicU64`, same pattern as `meta_hits`).

- [ ] **Step 2: Run** `cargo test -p ohara-engine -- ancestry run_branch filter_is_skipped` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(engine): per-worktree find_pattern visibility (red)"`
- [ ] **Step 4: Implement** per the flow above.
- [ ] **Step 5: Run** `cargo test -p ohara-engine` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(engine): ancestry-filtered find_pattern with LRU"`

**Acceptance:** `cargo test -p ohara-engine -- ancestry run_branch filter_is_skipped`

---

### Task 10: `explain_change` git fallback for unindexed commits (ohara-core, ohara-git, ohara-engine)

**Files:**
- Modify: `crates/ohara-core/src/explain/mod.rs` (new trait beside `BlameSource`, `:32-46`; orchestrator call site)
- Modify: `crates/ohara-core/src/explain/hydrator.rs:44-49` (`HydrateInputs`), `:112-160` (resolve loop), `build_limitation`
- Modify: `crates/ohara-git/src/blame.rs` (`impl CommitLookup for Blamer`)
- Modify: `crates/ohara-engine/src/engine.rs` (both `HydrateInputs` constructions in `explain_change`)
- Test: `crates/ohara-core/src/explain/tests.rs`, `crates/ohara-git/src/blame.rs` tests

**Interfaces — Produces:**

```rust
// ohara-core::explain
#[async_trait]
pub trait CommitLookup: Send + Sync {
    /// Metadata straight from git. `Ok(None)` when the object does not exist.
    async fn commit_meta(&self, sha: &str) -> Result<Option<CommitMeta>>;
}
pub struct HydrateInputs<'a> {
    pub storage: &'a dyn Storage,
    pub blame_ranges: Vec<BlameRange>,
    pub query: &'a ExplainQuery,
    pub repo_id: &'a RepoId,
    pub git_fallback: Option<&'a dyn CommitLookup>,   // new
}
```

Behaviour: for a blame SHA missing from storage, when `git_fallback` returns metadata, emit an `ExplainHit` with that author/date/message, `provenance: Exact`, `diff_excerpt: ""`, `diff_truncated: false`. Such lines count toward `coverage`. `limitation` is then exactly:
`"{n} commit(s) not yet indexed: metadata from git, no diff excerpt ({shas})"` with SHAs abbreviated to 7 characters and comma-separated. SHAs the fallback cannot resolve keep today's skip-and-report behaviour. `ExplainHit`'s shape does not change.

- [ ] **Step 1: Write the failing tests** in `explain/tests.rs` (existing `MockStorage`; add `MapLookup(HashMap<String, CommitMeta>)`):

```rust
#[tokio::test]
async fn unindexed_blame_commit_is_hydrated_from_git() {
    // storage knows sha A; blame returns ranges for A (lines 1-2) and U (line 3); lookup knows U
    // hits.len() == 2; the U hit has U's message/author, diff_excerpt == "", provenance Exact
    // coverage == 1.0
    // limitation == Some("1 commit(s) not yet indexed: metadata from git, no diff excerpt (<U[..7]>)")
}

#[tokio::test]
async fn commit_unknown_to_both_storage_and_git_is_still_skipped() {
    // lookup empty -> same hits/coverage/limitation as with git_fallback: None
}

#[tokio::test]
async fn no_fallback_keeps_legacy_behaviour() { /* git_fallback: None */ }
```

`blame.rs`:

```rust
#[tokio::test]
async fn blamer_commit_meta_reads_git_and_returns_none_for_unknown_sha() {}
```

- [ ] **Step 2: Run** `cargo test -p ohara-core explain && cargo test -p ohara-git blame` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(core): explain falls back to git metadata for unindexed commits (red)"`
- [ ] **Step 4: Implement.** `Blamer::commit_meta` runs in `spawn_blocking` and reuses `walker::commit_meta` (Task 4; make it `pub(crate)`). The engine passes `Some(handle.blamer.as_ref())` at both call sites; the core orchestrator passes `None` unless its `BlameSource` argument is also a `CommitLookup` (leave it `None`).
- [ ] **Step 5: Run** `cargo test -p ohara-core explain && cargo test -p ohara-git && cargo test -p ohara-engine && cargo test -p ohara-mcp` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(core): explain_change git-metadata fallback"`

**Acceptance:** `cargo test -p ohara-core explain`

---

### Task 11: Prune and the daemon-side index service (ohara-storage, ohara-engine)

**Files:**
- Create: `crates/ohara-storage/src/tables/prune.rs`
- Modify: `crates/ohara-storage/src/tip_store.rs` (the two Task-11-marked stubs become real)
- Create: `crates/ohara-engine/src/index_service.rs`
- Modify: `crates/ohara-engine/src/lib.rs` (`mod index_service; pub use index_service::{EnsureIndexedOutcome, PruneOutcome};`), `crates/ohara-engine/src/engine.rs` (three thin methods), `crates/ohara-engine/src/error.rs` (nothing new: reuse `NoIndex` and `NeedsRebuild`)
- Test: `crates/ohara-storage/src/tip_store.rs`, `crates/ohara-engine/src/engine_tests.rs`

**Interfaces — Produces:**

```rust
// ohara-storage::tables::prune
pub fn list_commit_shas(c: &Connection) -> Result<Vec<String>>;
/// One transaction per call: vec_commit, fts_commit, the hunks' fts_hunk_text /
/// fts_hunk_semantic / vec_hunk rows, hunk (hunk_symbol cascades), commit_record.
pub fn delete_commits(c: &mut Connection, shas: &[String]) -> Result<u64>;

// ohara-engine
#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureIndexedOutcome { pub indexed: bool, pub new_commits: u64, pub elapsed_ms: u64 }
#[derive(Debug, Serialize, Deserialize)]
pub struct PruneOutcome { pub commits_deleted: u64, pub tips_removed: u64 }

impl RetrievalEngine {
    /// Blocks until `sha` is covered. `indexed == false` means it already was.
    pub async fn ensure_indexed(&self, repo_path: impl AsRef<Path>, sha: &CommitSha)
        -> crate::Result<EnsureIndexedOutcome>;
    /// Index this worktree's HEAD now (used by the queue worker and by ensure_indexed).
    pub async fn index_worktree(&self, repo_path: impl AsRef<Path>) -> crate::Result<EnsureIndexedOutcome>;
    pub async fn prune_unreachable(&self, repo_path: impl AsRef<Path>) -> crate::Result<PruneOutcome>;
}
```

Rules:

- **Covered** means: some indexed tip `t` has `visible(t, sha) == true` (use the engine's `CachedVisibility`).
- `ensure_indexed`: no tips at all → `EngineError::NoIndex` (a run never indexes from scratch). Covered → `{ indexed: false, new_commits: 0, … }`. Otherwise `index_worktree`, then re-check; still uncovered → `EngineError::Internal("commit {sha} is not reachable from this worktree's HEAD")`.
- `index_worktree`: refuse with `EngineError::NeedsRebuild` unless `compose_response_meta(..).compatibility` is `Compatible`. Build the `Indexer` the way `crates/ohara-cli/src/commands/index.rs:387-403` does (engine embedder, `TreeSitterAtomicExtractor`, `with_repo_root(worktree path)`, runtime metadata from `current_runtime_metadata`), with `EmbedMode::Diff` when stored `embed_input_mode == "diff"` and `EmbedMode::Semantic` otherwise, plus `with_tip_context`. Index jobs for one `RepoId` are serialised by a per-repo `tokio::sync::Mutex` in the service. Afterwards call `invalidate_repo` so caches refresh.
- `prune_unreachable`: `indexed_commit_shas` minus `ohara_git::reachable_commit_shas` → `delete_commits` in batches of 500; then remove each linked `indexed_tip` whose worktree name is not in `Repository::worktrees()`. The main tip is never removed.

- [ ] **Step 1: Write the failing tests.**

`tip_store.rs`:

```rust
#[tokio::test]
async fn delete_commits_removes_commit_hunks_vectors_and_fts_rows() {
    // put 2 commits with 1 hunk each; delete one;
    // commit_exists(deleted) == false; commit_exists(kept) == true;
    // row counts in hunk, vec_hunk, fts_hunk_text, vec_commit, fts_commit each dropped by exactly 1
    // indexed_commit_shas == [kept]
}
```

`engine_tests.rs`:

```rust
#[tokio::test]
async fn ensure_indexed_is_a_noop_when_the_sha_is_covered() {
    // main indexed; sha = main HEAD^ -> indexed == false; embedder call count unchanged
}

#[tokio::test]
async fn ensure_indexed_runs_one_incremental_index_for_an_uncovered_head() {
    // new commit in slot-1 -> indexed == true, new_commits == 1; a second call -> indexed == false
}

#[tokio::test]
async fn indexing_a_worktree_embeds_only_its_new_commits() {
    // main has N indexed commits; slot-1 adds 2. Counting embedder: the hunks embedded by
    // index_worktree(slot-1) belong to exactly those 2 commits
}

#[tokio::test]
async fn ensure_indexed_refuses_an_unindexed_repo() {
    // never-indexed repo -> Err(EngineError::NoIndex { .. })
}

#[tokio::test]
async fn daemon_indexing_refuses_an_incompatible_index() {
    // Review Focus 5: stored metadata with a different embedding model
    // -> Err(EngineError::NeedsRebuild { .. }) and no commit rows added
}

#[tokio::test]
async fn prune_deletes_commits_of_a_deleted_branch_and_its_tip() {
    // index slot-1 with 1 extra commit; remove the worktree and delete its branch;
    // prune -> commits_deleted == 1, tips_removed == 1; main's commits all still exist
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-storage delete_commits && cargo test -p ohara-engine -- ensure_indexed indexing_a_worktree daemon_indexing prune_` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(engine): ensure_indexed, worktree indexing and prune (red)"`
- [ ] **Step 4: Implement** per Rules. Keep all of it in `index_service.rs` (`pub(crate) struct IndexService` owned by `RetrievalEngine`); `engine.rs` gains only three delegating methods.
- [ ] **Step 5: Run** `cargo test -p ohara-storage && cargo test -p ohara-engine` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(engine): daemon-side worktree indexing and prune"`

**Acceptance:** `cargo test -p ohara-engine -- ensure_indexed indexing_a_worktree daemon_indexing prune_`

---

### Task 12: IPC methods and the background index queue (ohara-engine)

**Files:**
- Modify: `crates/ohara-engine/src/ipc/envelope.rs:7-17` (`RequestMethod`) and its tests
- Modify: `crates/ohara-engine/src/server.rs:101-185` (`dispatch`) and its tests
- Modify: `crates/ohara-engine/src/index_service.rs` (queue), `crates/ohara-engine/src/daemon.rs:49-` (`run_daemon_with_engine` starts and stops the worker)

**Interfaces — Consumes:** Task 11's three engine methods. **Produces:**

```rust
pub enum RequestMethod {
    // … existing variants …
    EnsureIndexed { sha: String },   // result: EnsureIndexedOutcome
    IndexTip,                        // result: {"queued": true}; returns before any indexing
    PruneUnreachable,                // result: PruneOutcome
}
impl RetrievalEngine {
    /// Queue this worktree's HEAD for indexing. Never blocks on indexing.
    /// A worktree already waiting in the queue is not queued twice.
    pub fn enqueue_index_tip(&self, repo_path: PathBuf) -> bool;   // false when coalesced
    /// Drain the queue until `stop` is cancelled. One job at a time.
    pub async fn run_index_queue(self: Arc<Self>, stop: CancellationToken);
}
```

All three methods require `repo_path` (same `error_response(…, "… requires repo_path")` pattern as `server.rs:106-116`). `EnsureIndexed` with a malformed sha returns `ErrorCode::InvalidRequest`. The queue is an unbounded `tokio::sync::mpsc` of worktree paths plus a `Mutex<HashSet<PathBuf>>` of pending entries, removed when a job *starts* (so a commit made mid-job re-queues). A failed job is logged once with `tracing::warn!` and dropped. `run_daemon_with_engine` spawns `run_index_queue` with the daemon's existing `stop` token and awaits its join handle on shutdown. Both new long-running requests call `engine.touch()` on completion so the idle watchdog does not fire mid-index.

- [ ] **Step 1: Write the failing tests.**

`envelope.rs`:

```rust
#[test]
fn ensure_indexed_round_trip_uses_snake_case_tag() {
    // {"id":1,"repo_path":"/r","method":"ensure_indexed","params":{"sha":"…"}} round-trips
}
#[test]
fn index_tip_and_prune_round_trip() { /* "index_tip", "prune_unreachable" */ }
```

`server.rs` (existing `round_trip` helper):

```rust
#[tokio::test]
async fn index_tip_requires_repo_path() {}
#[tokio::test]
async fn ensure_indexed_rejects_a_malformed_sha() { /* ErrorCode::InvalidRequest */ }
```

`engine_tests.rs`:

```rust
#[tokio::test]
async fn index_tip_returns_before_indexing_and_the_queue_catches_up() {
    // slow counting embedder (50 ms per batch). enqueue_index_tip(slot-1) returns in < 20 ms;
    // spawn run_index_queue; poll until slot-1 tip == slot-1 HEAD (timeout 5 s); cancel stop.
}

#[tokio::test]
async fn repeated_index_tip_for_one_worktree_is_coalesced() {
    // no worker running: enqueue x3 -> [true, false, false]
}

#[tokio::test]
async fn find_pattern_is_served_while_an_index_job_runs() {
    // start a slow index job; a concurrent find_pattern on main completes before the job does
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-engine -- ensure_indexed_round index_tip prune_round coalesced find_pattern_is_served` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(engine): EnsureIndexed/IndexTip/PruneUnreachable IPC and queue (red)"`
- [ ] **Step 4: Implement** per Interfaces. `server.rs` is at 206 non-test lines; factor the repeated "require repo_path" match into `fn require_repo_path(id, repo_path, method) -> Result<String, Response>` while adding the three arms.
- [ ] **Step 5: Run** `cargo test -p ohara-engine` (includes `tests/daemon_e2e_*.rs`) — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(engine): index IPC methods and coalescing index queue"`

**Acceptance:** `cargo test -p ohara-engine -- ensure_indexed index_tip prune_ coalesced find_pattern_is_served`

---

### Task 13: CLI on the shared store (ohara-cli)

**Files:**
- Modify: `crates/ohara-cli/src/commands/mod.rs:23-33` (`resolve_repo_id`)
- Modify: `crates/ohara-cli/src/commands/index.rs:202`, `:289-302` (incremental fast path), `:387-403` (indexer wiring)
- Modify: `crates/ohara-cli/src/commands/status.rs:92-96`, `crates/ohara-cli/src/commands/explain.rs:45`
- Test: create `crates/ohara-cli/tests/plan_33_worktree_e2e.rs`

**Interfaces — Consumes:** `RepoIdentity`, `TipContext`, `compute_worktree_index_status`, `GitCommitsBehind::count_hiding`. **Produces:**

```rust
/// Replaces the (RepoId, PathBuf, String) tuple.
pub fn resolve_repo_id<P: AsRef<Path>>(repo_path: P) -> Result<ohara_git::RepoIdentity>;
```

- `index`: `storage.open_repo(&id.repo_id, &id.id_path, &id.first_commit_sha)`; the indexer always gets `with_tip_context(TipContext { tips: storage.clone(), worktree: id.worktree.clone(), head })`; `with_repo_root`, `GitCommitSource::open` and `GitSymbolSource::open` use `id.worktree_path`. The `--incremental` fast path becomes "nothing to walk": `GitCommitsBehind::open(&id.worktree_path)?.count_hiding(<all tip shas>) == 0` → print `index up-to-date at {head}` and return, without loading the embedder. `--force` in a linked worktree does not clear HEAD symbols (print one line saying HEAD symbols follow the main worktree). `--rebuild` is unchanged.
- `status`: uses `compute_worktree_index_status`; when run in a linked worktree print one extra line `worktree: <name> (shared index)`.
- `index.rs` must not grow: net change is a handful of lines. If it would pass 480 non-test lines, move the fast-path block into `commands/index/up_to_date.rs`.

- [ ] **Step 1: Write the failing e2e test** (pattern and helpers from `crates/ohara-cli/tests/e2e_incremental.rs`; isolated `OHARA_HOME`; `--no-daemon`; `--embed-provider cpu`).

```rust
#[test]
fn worktree_shares_the_main_index_end_to_end() {
    // 1. main: 3 commits. `ohara index <main>` -> exit 0.
    // 2. `git worktree add <wt> -b feat`. `ohara status <wt>` -> stdout contains
    //    "commits_behind_head: 0" and "worktree: " ; exactly ONE directory exists under OHARA_HOME.
    // 3. commit in <wt> with message "add zebra crossing guard".
    //    `ohara index --incremental <wt>` -> stdout index summary reports 1 new commit.
    // 4. `ohara query --no-daemon --query "zebra crossing guard" <wt>`   -> output contains that sha.
    //    `ohara query --no-daemon --query "zebra crossing guard" <main>` -> output does not.
    // 5. `ohara index --incremental <main>` -> prints "index up-to-date at".
    // 6. `ohara explain <wt> --file <file> --start 1 --end 1` names the worktree commit.
}

#[test]
fn existing_index_survives_the_upgrade_without_reembedding() {
    // index main; record the commit_record row count and the repo dir name under OHARA_HOME;
    // re-run `ohara index --incremental <main>` -> "index up-to-date at", same dir, same row count.
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-cli --test plan_33_worktree_e2e` — Expected: FAIL at step 2 (two directories under `OHARA_HOME`, or "not indexed").
- [ ] **Step 3: Commit the red test** — `git commit -m "test(cli): worktree shares the main clone's index e2e (red)"`
- [ ] **Step 4: Implement** per the bullets above.
- [ ] **Step 5: Run** `cargo test -p ohara-cli` — Expected: all pass (every existing e2e included).
- [ ] **Step 6: Commit** — `git commit -m "feat(cli): index, status and explain resolve worktrees to the shared store"`

**Acceptance:** `cargo test -p ohara-cli --test plan_33_worktree_e2e`

---

### Task 14: Route incremental `ohara index` through a running daemon (ohara-cli)

Depends on the open question flagged at the top; implements the spec's default.

**Files:**
- Create: `crates/ohara-cli/src/commands/index/daemon_route.rs`
- Modify: `crates/ohara-cli/src/commands/index.rs` (`mod daemon_route;` and one call ahead of the embedder load, after the up-to-date fast path), `crates/ohara-cli/src/main.rs:78` (pass `no_daemon` into `commands::index::run`)
- Test: `crates/ohara-cli/src/commands/index/daemon_route.rs` tests, `crates/ohara-engine/tests/daemon_e2e_happy.rs`

**Interfaces — Produces:**

```rust
/// What the incremental path should do.
pub(super) enum Route { Daemon(PathBuf /* socket */), InProcess }

/// Pure decision: Daemon only when every condition holds.
pub(super) fn choose_route(
    incremental: bool, force: bool, rebuild: bool, no_daemon: bool,
    explicit_tuning: bool,            // any of --embed-provider/--threads/--workers/--commit-batch/--embed-batch/--embed-cache given
    alive_same_version_socket: Option<PathBuf>,
) -> Route;

/// Sends EnsureIndexed { sha: head }. Ok(None) means "fall back in-process".
pub(super) async fn index_via_daemon(socket: &Path, worktree: &Path, head: &str)
    -> Result<Option<ohara_engine::EnsureIndexedOutcome>>;
```

- A daemon is only *found* (`Registry::list_alive`, same-version record), never spawned, for indexing.
- One exception to `explicit_tuning`: `--embed-provider cpu` does not count, because the post-commit hook passes it (`commands/init.rs:27`) and the daemon embedder is CPU.
- Any IPC error, `NotImplemented`, or `NeedsRebuild` from the daemon → `Ok(None)` → the existing in-process path runs and reports as today.
- On success print `indexed {new_commits} commit(s) via daemon in {elapsed_ms} ms` and return a report with `new_commits` set; skip `notify_daemons_of_invalidation` (the daemon invalidated itself).

- [ ] **Step 1: Write the failing tests.**

`daemon_route.rs`:

```rust
#[test]
fn routes_to_daemon_only_for_plain_incremental_with_a_live_daemon() {
    let sock = Some(PathBuf::from("/tmp/s"));
    assert!(matches!(choose_route(true, false, false, false, false, sock.clone()), Route::Daemon(_)));
    assert!(matches!(choose_route(false, false, false, false, false, sock.clone()), Route::InProcess)); // full index
    assert!(matches!(choose_route(true, true, false, false, false, sock.clone()), Route::InProcess));   // --force
    assert!(matches!(choose_route(true, false, true, false, false, sock.clone()), Route::InProcess));   // --rebuild
    assert!(matches!(choose_route(true, false, false, true, false, sock.clone()), Route::InProcess));   // --no-daemon
    assert!(matches!(choose_route(true, false, false, false, true, sock), Route::InProcess));           // tuned
    assert!(matches!(choose_route(true, false, false, false, false, None), Route::InProcess));          // none alive
}
```

`daemon_e2e_happy.rs` (existing harness that starts a real daemon on a temp socket):

```rust
#[tokio::test]
async fn ensure_indexed_over_the_socket_indexes_a_new_commit() {
    // indexed fixture repo; add a commit; send EnsureIndexed{head} via Client::call_with_timeout
    // -> result.indexed == true, new_commits == 1; a FindPattern afterwards can return that commit
}
```

- [ ] **Step 2: Run** `cargo test -p ohara-cli daemon_route && cargo test -p ohara-engine --test daemon_e2e_happy ensure_indexed` — Expected: COMPILE ERROR.
- [ ] **Step 3: Commit the red tests** — `git commit -m "test(cli): incremental index routes through a live daemon (red)"`
- [ ] **Step 4: Implement** per Interfaces. Use a 10-minute IPC timeout for `EnsureIndexed`.
- [ ] **Step 5: Run** `cargo test -p ohara-cli && cargo test -p ohara-engine` — Expected: all pass.
- [ ] **Step 6: Commit** — `git commit -m "feat(cli): route incremental index through a running daemon"`

**Acceptance:** `cargo test -p ohara-cli daemon_route && cargo test -p ohara-engine --test daemon_e2e_happy ensure_indexed`

---

### Task 15: Docs, perf guard, eval, and the full gate

**Files:**
- Modify: `docs-book/src/architecture/indexing.md` (per-worktree tips, hide-all-tips walk, daemon as writer for incremental work), `docs-book/src/architecture/storage.md` (`indexed_tip`, `repo.last_indexed_commit` retired), `docs-book/src/architecture/retrieval-pipeline.md` (visibility filter and where it sits), `docs-book/src/cli/index.md` (worktrees share the index; `--incremental` may run in the daemon; `--force` and HEAD symbols in a linked worktree), `docs-book/src/cli/status.md` (`worktree:` line; per-worktree `commits_behind_head`), `docs-book/src/tools/explain_change.md` (git-metadata fallback and its `limitation` text), `docs-book/src/changelog.md` (Unreleased entry)
- Modify: `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md` — append a dated strike-through note to the "Indexing happens via the CLI, period." invariant, in the style of the existing plan-16 note in the same list
- Modify: `CLAUDE.md` and `CONTRIBUTING.md` §1 only if a crate-role sentence became false (the `ohara-engine` row: add "daemon-side incremental indexing")
- Create: `tests/perf/visibility_filter_bench.rs`; Modify: `tests/perf/Cargo.toml` (add a `[[test]]` entry in the style of the `context_engine_eval` one)

- [ ] **Step 1: Write the perf guard** (`#[ignore]`, operator-run, prints one line starting `perf::visibility_filter`). Fixture: the plan-10 eval repo (`tests/perf/build_context_eval_fixture.sh`) plus one linked worktree with 20 extra indexed commits. Measure 200 `find_pattern` calls from the main clone with the filter active and with it bypassed; print `p50_ms_on`, `p95_ms_on`, `p50_ms_off`, `p95_ms_off`, `added_p95_ms`. Assert `added_p95_ms < 5.0` (spec §13 phase 1a).
- [ ] **Step 2: Run it** — `cargo test -p ohara-perf-tests -- --ignored visibility_filter --nocapture` — Expected: PASS. If it fails on cold cache, the remedy is in `GitVisibility::visible`: compute the invisible set once per `(head, tips)` with a revwalk that pushes the tips and hides `head`, instead of one `graph_descendant_of` per candidate. Do not weaken the assertion.
- [ ] **Step 3: Run the retrieval eval** (CONTRIBUTING §14) — `cargo test -p ohara-perf-tests -- --ignored context_engine_eval --nocapture`. Expected: the `perf::context_engine_eval` summary line matches `main` (the filter is skipped when only the main tip exists). Paste both summary lines into the PR description.
- [ ] **Step 4: Commit** — `git commit -m "test(perf): visibility filter latency guard"`
- [ ] **Step 5: Update the docs** listed above. Each page states the behaviour, not the plan number history.
- [ ] **Step 6: Build the book** — `mdbook build docs-book` — Expected: no warnings about broken links. (Skip with a note if `mdbook` is not installed.)
- [ ] **Step 7: Commit** — `git commit -m "docs: shared worktree store, per-worktree tips and visibility"`
- [ ] **Step 8: Full gate** (CONTRIBUTING §13):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: no diff, no warnings, all green.

- [ ] **Step 9: Check the caps and the constraints**

```bash
for f in crates/ohara-storage/src/storage_impl.rs crates/ohara-cli/src/commands/index.rs \
         crates/ohara-engine/src/engine.rs crates/ohara-engine/src/server.rs \
         crates/ohara-core/src/indexer.rs crates/ohara-engine/src/index_service.rs; do
  n=$(grep -n -m1 '#\[cfg(test)\]' "$f" | cut -d: -f1); echo "${n:-$(wc -l < "$f")} $f"
done
git diff main --stat -- crates/ohara-storage/migrations/
```

Expected: every number under 500; the migrations diff shows only `V7__indexed_tip.sql` added.

**Acceptance:** the three commands in Step 8.

---

## Spec coverage

| Spec item (§3 / §13 phase 1a) | Task |
|---|---|
| `commondir`-aware `RepoId`, plain-clone ids unchanged | 2, 8, 13 |
| V7 `indexed_tip` with backfill | 3 |
| Multi-tip walker, missing tips skipped | 4, 5 |
| `CommitVisibility` in core, git impl, post-filter, LRU, fast path | 6, 7, 9 |
| `RepoHandle` split, per-worktree blamer and caches | 8 |
| `EnsureIndexed` / `IndexTip` / `PruneUnreachable`, single writer | 11, 12 |
| Incremental `ohara index` via the daemon | 14 |
| `explain_change` git fallback | 10 |
| Head-snapshot symbol lane stays on the main worktree | 5, 13 |
| Acceptance: embeds only the worktree's new commits | 11 |
| Acceptance: plan-10 eval unchanged, filter under 5 ms p95 | 15 |
| docs-book | 15 |
| `--rebuild` refuses while runs are active | not here (phase 1b: needs the run table) |
