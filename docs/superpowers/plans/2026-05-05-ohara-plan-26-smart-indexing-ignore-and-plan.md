# ohara plan-26 — Smart indexing: `ohara plan` + `.oharaignore` (Spec A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per repo
> conventions: commit after each red test and again after each green
> implementation.

**Goal:** Add `ohara plan [path]`, a pre-flight subcommand that walks
a repo's history (paths only, no diff text), prints a directory commit-
share hotmap, suggests a `.oharaignore`, and writes it at the repo root.
Wire a layered `IgnoreFilter` (built-in defaults + `.gitattributes`
linguist-* + `.oharaignore`) into the indexer's pipeline so ignored
paths skip parse + embed; commits whose changed paths are 100% ignored
are dropped entirely while their watermark advances.

**Architecture:**

- New module `crates/ohara-core/src/ignore.rs` — `IgnoreFilter` trait,
  `LayeredIgnore` impl, built-in defaults `pub const`. Uses the
  `ignore` crate (gitignore-syntax `Gitignore` matchers) which is
  added to `[workspace.dependencies]`.
- New helper `crates/ohara-git/src/walker.rs::for_each_commit_paths`
  — streaming, callback-based path-only walk used by `ohara plan`.
- Indexer wiring at `crates/ohara-core/src/indexer/coordinator/mod.rs`:
  the per-commit loop filters `HunkRecord`s by path before stages 3-5
  run; if zero records survive a non-empty diff, the commit is skipped
  (no `commit::put`, no rows) and the watermark still advances.
  `Indexer::run` loads `LayeredIgnore::load(repo_root)` and threads it
  to the `Coordinator`.
- New CLI command `crates/ohara-cli/src/commands/plan.rs` — clap args,
  hotmap aggregator, suggestion rules, marker-fenced `.oharaignore`
  writer (mirrors `init.rs` patterns).
- `ohara status` learns one new line: `ignore_rules: …`.

**Tech stack:** Rust 2021, existing `git2` / `tokio` / `clap`, plus the
new `ignore = "0.4"` crate. No new storage migrations.

**Spec:** `docs/superpowers/specs/2026-05-05-ohara-plan-and-ignore-design.md`.

**Sequencing:**

- Phase A (`IgnoreFilter` foundation) and Phase B (walker helper) are
  independent — two agents can pick them up in parallel.
- Phase C (indexer wiring) depends on A.
- Phase D (`ohara plan` command) depends on B (and reuses A's filter).
- Phase E (status + docs) depends on A + C.
- Phase F (integration tests) depends on everything.

This plan is independent of plans 27 (Spec B — chunk dedup, deferred)
and 28 (Spec D — parallel pipeline, deferred).

---

## Phase A — `IgnoreFilter` foundation

### Task A.1 — Add `ignore` crate to `[workspace.dependencies]`

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/ohara-core/Cargo.toml`

- [ ] **Step 1: Add the dep to the workspace table**

In `Cargo.toml`, in the `[workspace.dependencies]` section (after the
`lru` line), append:

```toml
ignore = "0.4"
```

- [ ] **Step 2: Reference it from `ohara-core`**

In `crates/ohara-core/Cargo.toml`, under `[dependencies]` (after
`futures.workspace = true`), append:

```toml
ignore = { workspace = true }
```

- [ ] **Step 3: Verify the workspace builds**

Run: `cargo build -p ohara-core`
Expected: builds clean (no compile error from the new dep).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/ohara-core/Cargo.toml
git commit -m "chore(core): add ignore crate to workspace dependencies"
```

---

### Task A.2 — `IgnoreFilter` trait + `BUILT_IN_DEFAULTS` constant

**Files:**
- Create: `crates/ohara-core/src/ignore.rs`
- Modify: `crates/ohara-core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/ohara-core/src/ignore.rs` with the test shell:

```rust
//! Path-aware indexing filter (plan-26).
//!
//! Three layers, lower number wins (i.e., `!negate` in `.oharaignore`
//! overrides a `BUILT_IN_DEFAULTS` match):
//!   1. Built-in defaults (compiled in, see [`BUILT_IN_DEFAULTS`]).
//!   2. `.gitattributes` `linguist-generated=true` / `linguist-vendored=true`.
//!   3. User `.oharaignore` at repo root.

/// Patterns ohara always wants ignored unless the user negates with `!`.
/// Updated by spec A; further additions go through code review like
/// any other heuristic.
pub const BUILT_IN_DEFAULTS: &[&str] = &[
    // Lockfiles.
    "*.lock",
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Pipfile.lock",
    "poetry.lock",
    "go.sum",
    // Vendored / generated dirs.
    "node_modules/",
    "vendor/",
    "target/",
    "dist/",
    "build/",
    ".next/",
    "__pycache__/",
    ".venv/",
    "venv/",
    // Misc generated artifacts.
    "*.min.js",
    "*.min.css",
];

/// Matcher contract used by the indexer and `ohara plan`.
pub trait IgnoreFilter: Send + Sync {
    /// Returns `true` when `path` (repo-relative) is excluded from indexing.
    fn is_ignored(&self, path: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_defaults_includes_lockfiles_and_vendor_dirs() {
        let s: std::collections::HashSet<&&str> = BUILT_IN_DEFAULTS.iter().collect();
        assert!(s.contains(&"Cargo.lock"));
        assert!(s.contains(&"node_modules/"));
        assert!(s.contains(&"target/"));
        assert!(s.contains(&"vendor/"));
    }
}
```

In `crates/ohara-core/src/lib.rs`, add to the module list (alphabetical,
right after `pub mod hunk_text;`):

```rust
pub mod ignore;
```

And re-export at the bottom (after the `pub use index_metadata` block):

```rust
pub use ignore::{IgnoreFilter, BUILT_IN_DEFAULTS};
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p ohara-core ignore::tests::built_in_defaults`
Expected: PASS (the constant is the implementation; this is a sanity
test pinning the contract).

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-core/src/ignore.rs crates/ohara-core/src/lib.rs
git commit -m "feat(core): add IgnoreFilter trait + BUILT_IN_DEFAULTS list"
```

---

### Task A.3 — `LayeredIgnore` skeleton + builtins-only matching

**Files:**
- Modify: `crates/ohara-core/src/ignore.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/ohara-core/src/ignore.rs`:

```rust
    #[test]
    fn builtins_only_matches_lockfile_at_root() {
        // Plan 26 Task A.3: a LayeredIgnore with only the built-in layer
        // must match `Cargo.lock` at the repo root.
        let f = LayeredIgnore::builtins_only();
        assert!(f.is_ignored("Cargo.lock"));
    }

    #[test]
    fn builtins_only_matches_node_modules_subpath() {
        // Plan 26 Task A.3: directory pattern `node_modules/` must match
        // any path beneath it.
        let f = LayeredIgnore::builtins_only();
        assert!(f.is_ignored("packages/foo/node_modules/lodash/index.js"));
    }

    #[test]
    fn builtins_only_does_not_match_real_source() {
        let f = LayeredIgnore::builtins_only();
        assert!(!f.is_ignored("src/main.rs"));
        assert!(!f.is_ignored("crates/ohara-core/src/lib.rs"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core ignore::tests`
Expected: FAIL with "cannot find type LayeredIgnore" (and similar).

- [ ] **Step 3: Implement the minimal struct**

Append to `crates/ohara-core/src/ignore.rs` (above the `#[cfg(test)]`
module):

```rust
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

/// Layered filter: built-ins, `.gitattributes`, user `.oharaignore`.
///
/// Matchers are constructed once at index start; `is_ignored` is a
/// pure lookup on the resulting `Gitignore` matchers.
pub struct LayeredIgnore {
    builtins: Gitignore,
    gitattributes: Gitignore,
    user: Gitignore,
}

impl LayeredIgnore {
    /// Builder used by tests; no `.gitattributes`, no user file.
    pub fn builtins_only() -> Self {
        let builtins = build_gitignore_from_patterns(Path::new("/"), BUILT_IN_DEFAULTS)
            .expect("invariant: built-in defaults are valid gitignore patterns");
        Self {
            builtins,
            gitattributes: Gitignore::empty(),
            user: Gitignore::empty(),
        }
    }
}

impl IgnoreFilter for LayeredIgnore {
    fn is_ignored(&self, path: &str) -> bool {
        // User `.oharaignore` wins over earlier layers (so `!negate` works).
        // Any matcher's `Whitelist` (i.e., `!pattern`) short-circuits to
        // "not ignored"; any `Ignore` to "ignored". `None` falls through.
        let p = Path::new(path);
        for layer in [&self.user, &self.gitattributes, &self.builtins] {
            let m = layer.matched(p, false);
            if m.is_whitelist() {
                return false;
            }
            if m.is_ignore() {
                return true;
            }
        }
        false
    }
}

/// Build a `Gitignore` matcher from in-memory patterns rooted at `root`.
fn build_gitignore_from_patterns(root: &Path, patterns: &[&str]) -> Result<Gitignore, ignore::Error> {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        b.add_line(None, p)?;
    }
    b.build()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-core ignore::tests`
Expected: PASS (4 tests including the constant sanity test from A.2).

- [ ] **Step 5: Commit (red+green together since the failing tests
      were committed alongside the matching impl)**

```bash
git add crates/ohara-core/src/ignore.rs
git commit -m "feat(core): LayeredIgnore::builtins_only with gitignore-syntax matching"
```

---

### Task A.4 — `.gitattributes` linguist-* layer

**Files:**
- Modify: `crates/ohara-core/src/ignore.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    #[test]
    fn gitattributes_linguist_generated_is_ignored() {
        // Plan 26 Task A.4: a path flagged `linguist-generated=true` in
        // .gitattributes must be ignored even if the user has no
        // .oharaignore.
        let attrs = "src/generated.rs linguist-generated=true\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, attrs, "");
        assert!(f.is_ignored("src/generated.rs"));
    }

    #[test]
    fn gitattributes_linguist_vendored_is_ignored() {
        let attrs = "third_party/** linguist-vendored=true\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, attrs, "");
        assert!(f.is_ignored("third_party/foo/bar.c"));
    }

    #[test]
    fn gitattributes_unrelated_attribute_is_not_a_signal() {
        // Plan 26 Task A.4: only linguist-generated and linguist-vendored
        // affect the ignore-set. `text=auto` etc. must NOT mark a path
        // as ignored.
        let attrs = "src/foo.rs text=auto\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, attrs, "");
        assert!(!f.is_ignored("src/foo.rs"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core ignore::tests`
Expected: FAIL with "no function or associated item named `from_strings`".

- [ ] **Step 3: Implement `from_strings` + the gitattributes parser**

Add to `crates/ohara-core/src/ignore.rs` (above the existing
`build_gitignore_from_patterns` helper):

```rust
impl LayeredIgnore {
    /// Test/programmatic constructor: pass the three layers as in-memory
    /// strings. Used by unit tests and by `LayeredIgnore::load`.
    pub fn from_strings(builtins: &[&str], gitattributes: &str, user_oharaignore: &str) -> Self {
        let root = Path::new("/");
        let builtins = build_gitignore_from_patterns(root, builtins)
            .expect("invariant: built-in defaults are valid gitignore patterns");
        let gitattributes = build_gitignore_from_gitattributes(root, gitattributes);
        let user = build_gitignore_from_lines(root, user_oharaignore);
        Self {
            builtins,
            gitattributes,
            user,
        }
    }
}

/// Parse a `.gitattributes` string and emit a `Gitignore` matcher
/// covering paths flagged `linguist-generated=true` or
/// `linguist-vendored=true`. Lines without those attributes are
/// ignored. Patterns are reused verbatim — gitattributes path patterns
/// are gitignore-compatible.
fn build_gitignore_from_gitattributes(root: &Path, contents: &str) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let pattern = match tokens.next() {
            Some(p) => p,
            None => continue,
        };
        let flags_active = tokens.any(|t| {
            t == "linguist-generated=true"
                || t == "linguist-generated"
                || t == "linguist-vendored=true"
                || t == "linguist-vendored"
        });
        if !flags_active {
            continue;
        }
        // gitattributes wildcards are gitignore-compatible.
        let _ = b.add_line(None, pattern);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Parse a `.oharaignore` (gitignore-syntax) string into a matcher.
fn build_gitignore_from_lines(root: &Path, contents: &str) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let _ = b.add_line(None, line);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-core ignore::tests`
Expected: PASS (all gitattributes tests + earlier tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/ignore.rs
git commit -m "feat(core): LayeredIgnore honors .gitattributes linguist-*"
```

---

### Task A.5 — User `.oharaignore` layer + `!negate` precedence

**Files:**
- Modify: `crates/ohara-core/src/ignore.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    #[test]
    fn user_pattern_ignores_path() {
        // Plan 26 Task A.5: a pattern in the user `.oharaignore` must
        // ignore matching paths.
        let user = "drivers/\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, "", user);
        assert!(f.is_ignored("drivers/staging/foo.c"));
    }

    #[test]
    fn user_negate_overrides_builtin() {
        // Plan 26 Task A.5: user `!Cargo.lock` must un-ignore a path
        // that the BUILT_IN_DEFAULTS would ignore. The `!` negation has
        // to win over the builtin layer.
        let user = "!Cargo.lock\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, "", user);
        assert!(!f.is_ignored("Cargo.lock"));
    }

    #[test]
    fn user_negate_overrides_gitattributes() {
        // Plan 26 Task A.5: same precedence story for the gitattributes
        // layer — user `!` wins.
        let attrs = "generated.rs linguist-generated=true\n";
        let user = "!generated.rs\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, attrs, user);
        assert!(!f.is_ignored("generated.rs"));
    }

    #[test]
    fn comments_and_blank_lines_in_user_file_are_skipped() {
        let user = "# comment\n\n   \nlibs/\n";
        let f = LayeredIgnore::from_strings(BUILT_IN_DEFAULTS, "", user);
        assert!(f.is_ignored("libs/foo.rs"));
        // The blank/comment lines must not have produced any matchers
        // that affect unrelated paths.
        assert!(!f.is_ignored("src/main.rs"));
    }
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p ohara-core ignore::tests`
Expected: PASS — the user-layer code path was already implemented in
Task A.4 (the priority order in `is_ignored` puts `user` first, which
naturally lets `!negate` win). These tests pin that contract.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-core/src/ignore.rs
git commit -m "test(core): pin .oharaignore user-layer precedence over builtins/gitattributes"
```

---

### Task A.6 — `LayeredIgnore::load(repo_root)` constructor

**Files:**
- Modify: `crates/ohara-core/src/ignore.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    #[test]
    fn load_with_no_files_yields_builtins_only() {
        // Plan 26 Task A.6: load() on an empty dir must succeed and
        // behave like builtins_only.
        let dir = tempfile::tempdir().expect("tempdir");
        let f = LayeredIgnore::load(dir.path()).expect("load empty repo");
        assert!(f.is_ignored("node_modules/foo.js"));
        assert!(!f.is_ignored("src/main.rs"));
    }

    #[test]
    fn load_reads_oharaignore_at_repo_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".oharaignore"), "drivers/\n")
            .expect("write .oharaignore");
        let f = LayeredIgnore::load(dir.path()).expect("load");
        assert!(f.is_ignored("drivers/foo.c"));
    }

    #[test]
    fn load_reads_gitattributes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "generated.rs linguist-generated=true\n",
        )
        .expect("write .gitattributes");
        let f = LayeredIgnore::load(dir.path()).expect("load");
        assert!(f.is_ignored("generated.rs"));
    }
```

Add `tempfile` to `[dev-dependencies]` in
`crates/ohara-core/Cargo.toml` (after the existing
`tracing-subscriber.workspace = true`):

```toml
tempfile = "3"
```

(Add `tempfile = "3"` to `[workspace.dependencies]` in the root
`Cargo.toml` if not already present, and use `tempfile.workspace = true`
form in the crate `Cargo.toml`.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core ignore::tests::load_`
Expected: FAIL with "no function or associated item named `load`".

- [ ] **Step 3: Implement `load`**

Add to the `impl LayeredIgnore { … }` block in
`crates/ohara-core/src/ignore.rs`:

```rust
    /// Load the three-layer filter from a repo root directory.
    ///
    /// Reads `<root>/.gitattributes` and `<root>/.oharaignore` if they
    /// exist; missing files are treated as empty (no error). The
    /// built-in defaults are always applied.
    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let gitattributes = read_to_string_or_empty(&repo_root.join(".gitattributes"))?;
        let user = read_to_string_or_empty(&repo_root.join(".oharaignore"))?;
        Ok(Self::from_strings(
            BUILT_IN_DEFAULTS,
            &gitattributes,
            &user,
        ))
    }
```

Below `build_gitignore_from_lines`, add:

```rust
fn read_to_string_or_empty(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-core ignore::tests`
Expected: PASS (all ignore tests).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/ohara-core/Cargo.toml crates/ohara-core/src/ignore.rs
git commit -m "feat(core): LayeredIgnore::load(repo_root) reads .gitattributes + .oharaignore"
```

---

## Phase B — Walker path-only helper

### Task B.1 — `GitWalker::for_each_commit_paths` streaming walk

**Files:**
- Modify: `crates/ohara-git/src/walker.rs`

- [ ] **Step 1: Write the failing test**

Append to the existing `mod tests` in `crates/ohara-git/src/walker.rs`
(after `init_repo_with_commits`):

```rust
    #[test]
    fn for_each_commit_paths_visits_all_commits_in_topo_order() {
        // Plan 26 Task B.1: stream commit-path pairs to a callback in
        // topological-reverse order (oldest first). Each commit's path
        // list reflects the files changed in that commit.
        let dir = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_commits(dir.path(), &["c1", "c2", "c3"]);
        let walker = GitWalker::open(dir.path()).unwrap();

        let mut seen_msgs: Vec<String> = Vec::new();
        let mut seen_paths: Vec<Vec<String>> = Vec::new();
        walker
            .for_each_commit_paths(|meta, paths| {
                seen_msgs.push(meta.message.trim().to_string());
                seen_paths.push(paths.to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(seen_msgs, vec!["c1", "c2", "c3"]);
        // init_repo_with_commits writes f0.txt, f1.txt, f2.txt — one
        // per commit. Each commit changes exactly one file (no overlap).
        assert_eq!(seen_paths.len(), 3);
        assert!(seen_paths[0].iter().any(|p| p == "f0.txt"));
        assert!(seen_paths[1].iter().any(|p| p == "f1.txt"));
        assert!(seen_paths[2].iter().any(|p| p == "f2.txt"));
    }

    #[test]
    fn for_each_commit_paths_callback_error_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_commits(dir.path(), &["c1", "c2", "c3"]);
        let walker = GitWalker::open(dir.path()).unwrap();
        let mut visited = 0;
        let res = walker.for_each_commit_paths(|_, _| {
            visited += 1;
            if visited == 2 {
                Err(anyhow::anyhow!("stop"))
            } else {
                Ok(())
            }
        });
        assert!(res.is_err(), "expected callback error to propagate");
        assert_eq!(visited, 2, "must stop after callback error");
    }
```

If `tempfile` isn't already in `crates/ohara-git/Cargo.toml`'s
`[dev-dependencies]`, add it:

```toml
tempfile = { workspace = true }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-git for_each_commit_paths`
Expected: FAIL with "no method named `for_each_commit_paths`".

- [ ] **Step 3: Implement the streaming walk**

Add to the `impl GitWalker { … }` block in
`crates/ohara-git/src/walker.rs`, below `list_commits`:

```rust
    /// Stream `(CommitMeta, changed-paths)` pairs to a callback in
    /// topological-reverse (oldest-first) order. Used by `ohara plan`
    /// for the diff-only pre-flight walk on giant repos. Memory-bounded:
    /// no full Vec materialised.
    ///
    /// "Changed paths" = paths in the `Added`, `Deleted`, `Modified`,
    /// `Renamed`, or `Copied` deltas of the commit-vs-first-parent diff.
    /// Merge commits use first-parent diff (matches `list_commits`'s
    /// `parent_sha` choice). Initial commits diff against the empty tree.
    pub fn for_each_commit_paths<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(&CommitMeta, &[String]) -> Result<()>,
    {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.push_head()?;

        let mut paths_buf: Vec<String> = Vec::with_capacity(16);
        for oid in walk {
            let oid = oid?;
            let c = self.repo.find_commit(oid)?;
            let parent_sha = if c.parent_count() > 0 {
                c.parent(0).ok().map(|p| p.id().to_string())
            } else {
                None
            };
            let meta = CommitMeta {
                commit_sha: oid.to_string(),
                parent_sha: parent_sha.clone(),
                is_merge: c.parent_count() > 1,
                author: Some(c.author().name().unwrap_or("").to_string()).filter(|s| !s.is_empty()),
                ts: c.time().seconds(),
                message: c.message().unwrap_or("").to_string(),
            };

            paths_buf.clear();
            collect_changed_paths(&self.repo, &c, &mut paths_buf)?;
            callback(&meta, &paths_buf)?;
        }
        Ok(())
    }
}

fn collect_changed_paths(
    repo: &Repository,
    commit: &git2::Commit<'_>,
    out: &mut Vec<String>,
) -> Result<()> {
    let new_tree = commit.tree().context("commit tree")?;
    let old_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0).context("first parent")?.tree().context("parent tree")?)
    } else {
        None
    };

    // Path-only diff: no rename detection, no diff text, no hunks.
    let mut opts = git2::DiffOptions::new();
    opts.skip_binary_check(true).include_untracked(false);
    let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
        .context("diff_tree_to_tree paths-only")?;

    diff.foreach(
        &mut |delta, _progress| {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                out.push(p.to_string_lossy().into_owned());
            }
            true
        },
        None, // binary cb
        None, // hunk cb
        None, // line cb
    )
    .context("diff foreach")?;
    Ok(())
}
```

Note: `collect_changed_paths` is a free function below `impl GitWalker`,
so move the closing `}` of the impl block up before it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-git for_each_commit_paths`
Expected: PASS (both tests).

Run: `cargo test -p ohara-git` to confirm no existing test regresses.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-git/Cargo.toml crates/ohara-git/src/walker.rs
git commit -m "feat(git): GitWalker::for_each_commit_paths — streaming path-only walk"
```

---

## Phase C — Indexer wiring

### Task C.1 — Plumb `Option<Arc<dyn IgnoreFilter>>` into `Coordinator`

**Files:**
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to the existing `tests` module at the end of
`crates/ohara-core/src/indexer/coordinator/tests.rs` (a new test
function; keep all existing tests in place):

```rust
    #[tokio::test]
    async fn coordinator_with_ignore_filter_field_is_set() {
        // Plan 26 Task C.1: Coordinator must accept an optional
        // IgnoreFilter via builder and stash it. We don't yet wire the
        // filter into the loop — that's Task C.2.
        use crate::ignore::LayeredIgnore;
        use std::sync::Arc;

        let storage = make_test_storage().await;
        let embedder = make_test_embedder();
        let filter: Arc<dyn crate::IgnoreFilter> = Arc::new(LayeredIgnore::builtins_only());
        let coord = Coordinator::new(storage, embedder).with_ignore_filter(filter.clone());
        // The smoke test is that the builder type-checks and returns
        // Self. The filter's behaviour is exercised in C.2/C.3.
        let _ = coord;
    }
```

You'll need a `make_test_storage` / `make_test_embedder` helper if not
already in the tests module — reuse the existing pattern from
`coordinator/tests.rs`. Skip the test if the existing tests already use
`Coordinator::new`; in that case the helper exists.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-core coordinator_with_ignore_filter_field_is_set`
Expected: FAIL with "no method named `with_ignore_filter`".

- [ ] **Step 3: Add the field + builder**

In `crates/ohara-core/src/indexer/coordinator/mod.rs`, modify the
`Coordinator` struct (around line 54) to add the field:

```rust
pub struct Coordinator {
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
    progress: Arc<dyn ProgressSink>,
    ignore_filter: Option<Arc<dyn crate::IgnoreFilter>>,
}
```

Update `Coordinator::new` (around line 64) to default the new field to
`None`:

```rust
    pub fn new(
        storage: Arc<dyn Storage + Send + Sync>,
        embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    ) -> Self {
        Self {
            storage,
            embedder,
            embed_batch: 32,
            progress: Arc::new(NullProgress),
            ignore_filter: None,
        }
    }
```

Add the builder method below `with_progress`:

```rust
    /// Wire a [`LayeredIgnore`] (or any `IgnoreFilter` impl). When set,
    /// the per-commit pipeline drops `HunkRecord`s whose path matches
    /// the filter, and skips a commit entirely when 100% of its changed
    /// paths matched (advancing the watermark in either case).
    pub fn with_ignore_filter(mut self, f: Arc<dyn crate::IgnoreFilter>) -> Self {
        self.ignore_filter = Some(f);
        self
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-core coordinator_with_ignore_filter_field_is_set`
Expected: PASS.

Run: `cargo test -p ohara-core indexer::` to confirm existing
coordinator tests still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/indexer/coordinator/mod.rs crates/ohara-core/src/indexer/coordinator/tests.rs
git commit -m "feat(core): Coordinator::with_ignore_filter builder (no behaviour yet)"
```

---

### Task C.2 — Filter `HunkRecord`s by path before stages 3-5

**Files:**
- Modify: `crates/ohara-core/src/indexer/coordinator/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-core/src/indexer/coordinator/tests.rs`:

```rust
    #[tokio::test]
    async fn ignored_paths_drop_from_hunk_records_before_persist() {
        // Plan 26 Task C.2: when the filter ignores `vendor/foo.c`, the
        // pipeline must NOT persist a hunk for that path. The same
        // commit's `src/main.rs` hunk must still be persisted.
        use crate::ignore::LayeredIgnore;
        use std::sync::Arc;

        let storage = make_test_storage().await;
        let embedder = make_test_embedder();
        let filter: Arc<dyn crate::IgnoreFilter> =
            Arc::new(LayeredIgnore::from_strings(&[], "", "vendor/\n"));
        let coord = Coordinator::new(storage.clone(), embedder.clone())
            .with_ignore_filter(filter);

        let source = TwoFileMixedCommitSource;
        let symbol_source = NullSymbolSource;
        let repo = RepoId::from_components("/tmp/x", "0".repeat(40).as_str());
        coord.run(&repo, &source, &symbol_source).await.unwrap();

        // Persist surface: count the hunks that landed in storage.
        let hits = storage.list_hunks_for_commit("abc").await.unwrap();
        assert_eq!(
            hits.len(),
            1,
            "expected exactly 1 persisted hunk (the non-vendor one), got {}",
            hits.len()
        );
        assert_eq!(hits[0].file_path, "src/main.rs");
    }
```

You will likely need a small `TwoFileMixedCommitSource` test fixture
emitting one commit with two hunks (`src/main.rs` and `vendor/foo.c`),
plus a `NullSymbolSource` and a `list_hunks_for_commit` storage helper.
Reuse existing test scaffolding in `coordinator/tests.rs` if available;
add minimal new fixtures if not.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-core ignored_paths_drop_from_hunk_records`
Expected: FAIL — currently the coordinator persists both hunks because
no filter is applied.

- [ ] **Step 3: Apply the filter in `run_commit_timed`**

In `crates/ohara-core/src/indexer/coordinator/mod.rs`, modify
`run_commit_timed` (around line 209). After the `HunkChunkStage::run`
call (line 220) and before the `for rec in &records` loop, insert:

```rust
        // Plan 26 Task C.2: drop ignored paths before downstream stages.
        // Mixed commits keep their non-ignored hunks; pure-ignored
        // commits are caught by the `paths_kept == 0` branch below.
        let paths_total = records.len();
        if let Some(filter) = self.ignore_filter.as_ref() {
            records.retain(|r| !filter.is_ignored(&r.file_path));
        }
        let paths_kept = records.len();
        if paths_total > 0 && paths_kept == 0 {
            tracing::debug!(
                sha = %commit.commit_sha,
                "plan-26: commit has 100% ignored paths; skipping (watermark advances)"
            );
            return Ok(());
        }
```

To make `records` mutable, change the binding (line 220) from:

```rust
        let records = HunkChunkStage::run(commit_source, commit).await?;
```

to:

```rust
        let mut records = HunkChunkStage::run(commit_source, commit).await?;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ohara-core ignored_paths_drop_from_hunk_records`
Expected: PASS.

Run: `cargo test -p ohara-core indexer::` to confirm no existing test
regresses.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-core/src/indexer/coordinator/mod.rs crates/ohara-core/src/indexer/coordinator/tests.rs
git commit -m "feat(core): coordinator drops ignored hunks before parse/embed/persist"
```

---

### Task C.3 — 100%-ignored commit skip pins watermark advance

**Files:**
- Modify: `crates/ohara-core/src/indexer/coordinator/tests.rs`

- [ ] **Step 1: Write the failing test (regression pin)**

The early-return added in C.2 already implements the 100%-skip logic.
This task adds an explicit regression test that pins the watermark-
advance behaviour: even when the commit is 100% ignored, `latest_sha`
moves to that commit's SHA so subsequent `--incremental` runs don't
re-walk it.

Append to `crates/ohara-core/src/indexer/coordinator/tests.rs`:

```rust
    #[tokio::test]
    async fn fully_ignored_commit_advances_watermark_with_zero_persisted_rows() {
        // Plan 26 Task C.3: a commit whose changed paths are 100%
        // ignored must (a) persist zero hunks, (b) write no commit
        // metadata row, (c) still advance the coordinator's
        // `latest_sha` so `commits_behind_head` decreases on next run.
        use crate::ignore::LayeredIgnore;
        use std::sync::Arc;

        let storage = make_test_storage().await;
        let embedder = make_test_embedder();
        let filter: Arc<dyn crate::IgnoreFilter> =
            Arc::new(LayeredIgnore::from_strings(&[], "", "vendor/\n"));
        let coord = Coordinator::new(storage.clone(), embedder.clone())
            .with_ignore_filter(filter);

        let source = AllVendorCommitSource; // emits one commit, all vendor/ paths
        let symbol_source = NullSymbolSource;
        let repo = RepoId::from_components("/tmp/y", "0".repeat(40).as_str());
        let result = coord
            .run_timed(&repo, &source, &symbol_source)
            .await
            .unwrap();

        assert_eq!(result.new_commits, 0, "no new commit rows for skipped commit");
        assert_eq!(result.new_hunks, 0, "no hunks for skipped commit");
        assert_eq!(
            result.latest_sha.as_deref(),
            Some("abc"),
            "watermark must still advance to the skipped commit's SHA"
        );

        let persisted_hunks = storage.list_hunks_for_commit("abc").await.unwrap();
        assert!(persisted_hunks.is_empty());
    }
```

Now look at how the coordinator currently sets `latest_sha`. In
`crates/ohara-core/src/indexer/coordinator/mod.rs:184-194`:

```rust
            self.run_commit_timed(...).await?;
            result.latest_sha = Some(commit.commit_sha.clone());
```

The `latest_sha` advance happens regardless of what `run_commit_timed`
did. Our C.2 early return (`return Ok(())`) means
`run_commit_timed` returns success without touching `result.new_commits`
or `result.new_hunks`. So `latest_sha` advances correctly. **The test
should already pass after C.2**, but add it explicitly to pin the
contract.

You'll need an `AllVendorCommitSource` fixture: one commit whose hunks
are all under `vendor/`.

- [ ] **Step 2: Run the test**

Run: `cargo test -p ohara-core fully_ignored_commit_advances_watermark`
Expected: PASS (it pins existing behaviour from C.2's early return).

If it fails: the early-return in C.2 isn't propagating `latest_sha`
correctly. Adjust the coordinator main loop so `latest_sha` is
assigned *after* `run_commit_timed` whether or not it took the early-
return path (re-check line 193 of `mod.rs`).

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-core/src/indexer/coordinator/tests.rs
git commit -m "test(core): pin watermark advance for 100%-ignored commits"
```

---

### Task C.4 — `Indexer::run` loads `LayeredIgnore` from repo root

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [ ] **Step 1: Inspect the current `Indexer::run` shape**

Read `crates/ohara-core/src/indexer.rs` lines around 206 (the `pub async
fn run` definition spotted in earlier exploration). Identify the place
where `Coordinator` is constructed.

- [ ] **Step 2: Add a `repo_root: Option<PathBuf>` field + builder**

In `crates/ohara-core/src/indexer.rs`, find the `Indexer` struct
(line 118) and add:

```rust
    /// Repo root used to load `.oharaignore` / `.gitattributes`. When
    /// `None`, the coordinator runs without an ignore filter (today's
    /// behaviour).
    repo_root: Option<std::path::PathBuf>,
```

Add a builder method (mirroring `with_progress` etc.):

```rust
    /// Set the repo root from which `LayeredIgnore::load` reads
    /// `.gitattributes` and `.oharaignore`. Plan 26.
    pub fn with_repo_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }
```

Update `Indexer::new` to initialise `repo_root: None`.

In the body of `Indexer::run`, where the `Coordinator` is built, chain
in the ignore filter when a repo root is set. Approximate diff:

```rust
        let mut coord = Coordinator::new(self.storage.clone(), self.embedder.clone())
            .with_progress(self.progress.clone())
            .with_embed_batch(self.embed_batch);
        if let Some(root) = self.repo_root.as_ref() {
            // Best-effort load — a missing `.oharaignore` is fine.
            // Convert `std::io::Error` to the crate's error type using
            // whichever variant the existing code uses for filesystem
            // I/O (check `crates/ohara-core/src/error.rs` —
            // `From<std::io::Error>` likely already exists for
            // `OhraError`, in which case `?` is enough).
            let filter = crate::ignore::LayeredIgnore::load(root)?;
            coord = coord.with_ignore_filter(std::sync::Arc::new(filter));
        }
```

If `From<std::io::Error>` is not implemented for the core error type,
add it via `#[from]` on a new `Io(#[from] std::io::Error)` variant in
`crates/ohara-core/src/error.rs`, then re-run the build.

(Verify the exact `Coordinator` builder chain in the current code and
preserve any other `.with_*` calls.)

- [ ] **Step 3: Update `crates/ohara-cli/src/commands/index.rs`**

Find where `Indexer` is constructed (likely around `Indexer::new(...)`)
and chain `.with_repo_root(canonical.clone())` after the existing
builder calls. Use the canonicalized repo path that already exists in
the function (the same `canonical` used to build `RepoId`).

- [ ] **Step 4: Add a smoke test**

Add to `crates/ohara-core/src/indexer.rs` `#[cfg(test)]` module (or
create one if missing — see existing patterns):

```rust
    #[test]
    fn indexer_with_repo_root_stores_path() {
        use std::path::PathBuf;
        let storage: std::sync::Arc<dyn crate::Storage> = make_test_storage_sync();
        let embedder: std::sync::Arc<dyn crate::EmbeddingProvider> =
            make_test_embedder_sync();
        let i = Indexer::new(storage, embedder)
            .with_repo_root(PathBuf::from("/tmp/example"));
        assert_eq!(i.repo_root.as_deref(), Some(std::path::Path::new("/tmp/example")));
    }
```

If `make_test_storage_sync` / `make_test_embedder_sync` don't exist,
gate the test on the existing test helpers — the goal is just to pin
that the builder stores the path.

- [ ] **Step 5: Run tests**

Run: `cargo test -p ohara-core indexer::`
Expected: PASS.

Run: `cargo build -p ohara-cli` to confirm the CLI side compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/indexer.rs crates/ohara-cli/src/commands/index.rs
git commit -m "feat(core): Indexer loads LayeredIgnore from repo root when set"
```

---

## Phase D — `ohara plan` CLI subcommand

### Task D.1 — `plan.rs` skeleton + clap Args

**Files:**
- Create: `crates/ohara-cli/src/commands/plan.rs`
- Modify: `crates/ohara-cli/src/commands/mod.rs`

- [ ] **Step 1: Write the skeleton**

Create `crates/ohara-cli/src/commands/plan.rs`:

```rust
//! `ohara plan` — pre-flight planner that surveys the repo, prints a
//! directory commit-share hotmap, and writes a `.oharaignore` at the
//! repo root.
//!
//! Plan-26 / Spec A. The file lives at the repo root (not `.ohara/`)
//! so it's checked into the repo and shared across the team like
//! `.gitignore`.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Write `.oharaignore` without prompting.
    #[arg(long)]
    pub yes: bool,
    /// Print suggestions only; never write a file.
    #[arg(long, conflicts_with = "yes")]
    pub no_write: bool,
    /// Replace the entire `.oharaignore` (default: replace only the
    /// auto-generated section between markers, preserving user lines).
    #[arg(long)]
    pub replace: bool,
}

pub async fn run(_args: Args) -> Result<()> {
    Err(anyhow::anyhow!("plan-26: `ohara plan` not yet implemented"))
}
```

In `crates/ohara-cli/src/commands/mod.rs`, register the module
(alphabetical with the others):

```rust
pub mod plan;
```

- [ ] **Step 2: Verify the file compiles**

Run: `cargo build -p ohara-cli`
Expected: builds clean. (The `run` function intentionally returns an
"unimplemented" error — that's filled in by Tasks D.2-D.7.)

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/src/commands/plan.rs crates/ohara-cli/src/commands/mod.rs
git commit -m "feat(cli): scaffold `ohara plan` command (clap args only)"
```

---

### Task D.2 — Directory hotmap aggregator (pure function)

**Files:**
- Modify: `crates/ohara-cli/src/commands/plan.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-cli/src/commands/plan.rs`:

```rust
#[cfg(test)]
mod aggregator_tests {
    use super::*;

    #[test]
    fn aggregator_counts_commits_per_top_level_dir() {
        // Plan 26 Task D.2: each path bumps the counter for every prefix.
        // For `drivers/staging/foo.c` we increment `drivers/` by 1,
        // `drivers/staging/` by 1, and `drivers/staging/foo.c` by 1.
        // A second commit touching `drivers/usb/bar.c` bumps `drivers/`
        // again and the new prefixes.
        let mut agg = HotmapAggregator::default();
        agg.record(&["drivers/staging/foo.c".into()]);
        agg.record(&["drivers/usb/bar.c".into()]);
        agg.record(&["src/main.rs".into()]);

        let counts = agg.counts();
        assert_eq!(counts.get("drivers/"), Some(&2));
        assert_eq!(counts.get("drivers/staging/"), Some(&1));
        assert_eq!(counts.get("drivers/usb/"), Some(&1));
        assert_eq!(counts.get("src/"), Some(&1));
    }

    #[test]
    fn aggregator_total_commits_advances_per_record() {
        let mut agg = HotmapAggregator::default();
        agg.record(&["a.rs".into()]);
        agg.record(&["b.rs".into()]);
        agg.record(&[]); // empty diff still counts as a commit
        assert_eq!(agg.total_commits(), 3);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-cli aggregator_tests`
Expected: FAIL — `HotmapAggregator` is undefined.

- [ ] **Step 3: Implement `HotmapAggregator`**

Append to `crates/ohara-cli/src/commands/plan.rs`:

```rust
use std::collections::BTreeMap;

/// Streaming aggregator: receives `(commit, paths)` and tallies a
/// commit-count per directory prefix. Pure function over its inputs;
/// holds at most O(unique-prefixes) memory.
#[derive(Default)]
pub struct HotmapAggregator {
    counts: BTreeMap<String, u64>,
    total: u64,
}

impl HotmapAggregator {
    /// Record one commit's changed-paths list. Each prefix of each path
    /// is incremented once per commit (a commit touching two files
    /// under `drivers/` still bumps `drivers/` only once).
    pub fn record(&mut self, paths: &[String]) {
        self.total += 1;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in paths {
            let mut buf = String::new();
            for component in p.split('/') {
                if !buf.is_empty() {
                    buf.push('/');
                }
                buf.push_str(component);
                let key = if buf.contains('/') && !buf.ends_with('/') && p.starts_with(&format!("{buf}/")) {
                    format!("{buf}/")
                } else {
                    buf.clone()
                };
                if seen.insert(key.clone()) {
                    *self.counts.entry(key).or_insert(0) += 1;
                }
            }
        }
    }

    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    pub fn total_commits(&self) -> u64 {
        self.total
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ohara-cli aggregator_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/plan.rs
git commit -m "feat(cli): HotmapAggregator — directory commit-count tally"
```

---

### Task D.3 — Suggestion generator (rules)

**Files:**
- Modify: `crates/ohara-cli/src/commands/plan.rs`

- [ ] **Step 1: Write the failing test**

Append to `plan.rs`:

```rust
#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn high_share_directory_outside_docs_allowlist_is_suggested() {
        // Plan 26 Task D.3: a top-level directory with > 5% commit
        // share that isn't in the docs allowlist is suggested for IGNORE.
        let mut agg = HotmapAggregator::default();
        for _ in 0..70 { agg.record(&["drivers/foo.c".into()]); }
        for _ in 0..30 { agg.record(&["src/main.rs".into()]); }

        let suggestions = suggest_patterns(&agg);
        assert!(suggestions.iter().any(|p| p == "drivers/"));
        assert!(!suggestions.iter().any(|p| p == "src/"));
    }

    #[test]
    fn high_share_documentation_dir_is_kept() {
        // Plan 26 Task D.3: `Documentation/` is in the docs allowlist —
        // even at high commit share it must not be suggested for ignore.
        let mut agg = HotmapAggregator::default();
        for _ in 0..70 { agg.record(&["Documentation/foo.txt".into()]); }
        for _ in 0..30 { agg.record(&["src/main.rs".into()]); }
        let suggestions = suggest_patterns(&agg);
        assert!(!suggestions.iter().any(|p| p == "Documentation/"));
    }

    #[test]
    fn low_share_directory_not_suggested() {
        let mut agg = HotmapAggregator::default();
        for _ in 0..2 { agg.record(&["niche/foo.rs".into()]); }
        for _ in 0..98 { agg.record(&["src/main.rs".into()]); }
        let suggestions = suggest_patterns(&agg);
        assert!(!suggestions.iter().any(|p| p == "niche/"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-cli suggestion_tests`
Expected: FAIL — `suggest_patterns` is undefined.

- [ ] **Step 3: Implement the rules**

Append to `plan.rs`:

```rust
/// Top-level directory names the planner never suggests ignoring.
const DOCS_ALLOWLIST: &[&str] = &[
    "Documentation/",
    "docs/",
    "doc/",
];

/// Default share threshold for "high-share" suggestions, expressed as
/// a fraction of total commits. Tunable; 5% balances signal vs noise on
/// repos in the 100k+ commit range.
const HIGH_SHARE_THRESHOLD: f64 = 0.05;

/// Generate `.oharaignore` patterns from a populated aggregator. Top-
/// level directories with commit share above the threshold and not in
/// the docs allowlist are returned in deterministic order.
pub fn suggest_patterns(agg: &HotmapAggregator) -> Vec<String> {
    if agg.total_commits() == 0 {
        return Vec::new();
    }
    let threshold = (agg.total_commits() as f64 * HIGH_SHARE_THRESHOLD) as u64;
    let mut out: Vec<String> = Vec::new();

    for (key, count) in agg.counts() {
        // Top-level only: exactly one slash, at the end.
        let slash_count = key.matches('/').count();
        let is_toplevel_dir = slash_count == 1 && key.ends_with('/');
        if !is_toplevel_dir {
            continue;
        }
        if DOCS_ALLOWLIST.iter().any(|d| *d == key) {
            continue;
        }
        if *count >= threshold {
            out.push(key.clone());
        }
    }
    out.sort();
    out
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ohara-cli suggestion_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/plan.rs
git commit -m "feat(cli): plan suggestion rules — high-share top-levels minus docs allowlist"
```

---

### Task D.4 — `.oharaignore` writer with marker fences

**Files:**
- Modify: `crates/ohara-cli/src/commands/plan.rs`

- [ ] **Step 1: Write the failing tests**

Append to `plan.rs`:

```rust
#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn render_oharaignore_wraps_patterns_in_markers() {
        // Plan 26 Task D.4: the auto-generated section is fenced by
        // begin/end markers so re-runs replace only that block.
        let body = render_oharaignore_body(&["drivers/".into(), "vendor/".into()], "0.7.7");
        assert!(body.contains(MARKER_BEGIN));
        assert!(body.contains(MARKER_END));
        assert!(body.contains("drivers/"));
        assert!(body.contains("vendor/"));
        // The opening marker must include the version so a future ohara
        // can detect schema drift.
        assert!(body.contains("ohara plan v0.7.7"));
    }

    #[test]
    fn merge_replaces_only_auto_section_in_existing_file() {
        // Plan 26 Task D.4: --keep-existing (default) preserves user
        // lines outside the markers across re-runs.
        let existing = "\
# === ohara plan v0.7.6 — auto-generated 2026-05-04T12:00:00 ===
old_pattern/
# === end auto-generated ===

# user added below
my_team/
!Cargo.lock
";
        let new_section = render_oharaignore_body(&["drivers/".into()], "0.7.7");
        let merged = merge_oharaignore(existing, &new_section).expect("merge");

        assert!(merged.contains("drivers/"), "new pattern present");
        assert!(!merged.contains("old_pattern/"), "old auto pattern dropped");
        assert!(merged.contains("my_team/"), "user line preserved");
        assert!(merged.contains("!Cargo.lock"), "user negation preserved");
    }

    #[test]
    fn merge_fails_open_when_markers_missing() {
        // Plan 26 Task D.4: refusing to overwrite an existing file
        // without markers protects user lines from silent loss.
        let existing = "user_only_pattern/\n";
        let new_section = render_oharaignore_body(&["drivers/".into()], "0.7.7");
        let res = merge_oharaignore(existing, &new_section);
        assert!(res.is_err(), "merge must refuse when markers absent");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ohara-cli writer_tests`
Expected: FAIL — `render_oharaignore_body`, `merge_oharaignore`,
`MARKER_BEGIN`, `MARKER_END` are undefined.

- [ ] **Step 3: Implement the writer**

Append to `plan.rs`:

```rust
const MARKER_BEGIN_PREFIX: &str = "# === ohara plan v";
const MARKER_END: &str = "# === end auto-generated ===";

/// Public for tests; the live opener prepended in `render_oharaignore_body`.
pub const MARKER_BEGIN: &str = "# === ohara plan v";

/// Render the body of a fresh `.oharaignore`: marker-fenced patterns
/// followed by a hint for user-added lines below the closing marker.
pub fn render_oharaignore_body(patterns: &[String], version: &str) -> String {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let mut out = String::new();
    out.push_str(&format!(
        "{MARKER_BEGIN_PREFIX}{version} — auto-generated {timestamp} ===\n"
    ));
    for p in patterns {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str(MARKER_END);
    out.push('\n');
    out.push('\n');
    out.push_str("# user-added lines below this marker are preserved by `ohara plan --keep-existing`\n");
    out
}

/// Merge a freshly-rendered auto-section with an existing
/// `.oharaignore`. Replaces only the block between the markers; lines
/// outside are kept verbatim. Errors if the existing file is non-empty
/// and lacks markers (fail-open: refuse to silently overwrite user
/// content).
pub fn merge_oharaignore(existing: &str, new_section: &str) -> Result<String> {
    let trimmed = existing.trim();
    if trimmed.is_empty() {
        return Ok(new_section.to_string());
    }
    let begin = existing
        .find(MARKER_BEGIN_PREFIX)
        .ok_or_else(|| anyhow::anyhow!(
            "existing .oharaignore has content but no auto-generated markers; \
             pass --replace to overwrite or delete the file and re-run"
        ))?;
    let end = existing
        .find(MARKER_END)
        .ok_or_else(|| anyhow::anyhow!(
            "existing .oharaignore has begin marker but no end marker; refusing to merge"
        ))?
        + MARKER_END.len();

    // Walk past trailing whitespace/newline of the end marker line.
    let after_end = existing[end..]
        .find('\n')
        .map(|i| end + i + 1)
        .unwrap_or(end);

    let prefix = &existing[..begin];
    let suffix = &existing[after_end..];

    let mut out = String::new();
    out.push_str(prefix);
    out.push_str(new_section);
    out.push_str(suffix);
    Ok(out)
}
```

You'll need `chrono` in `crates/ohara-cli/Cargo.toml` `[dependencies]`
if it isn't there:

```toml
chrono = { workspace = true }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-cli writer_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/Cargo.toml crates/ohara-cli/src/commands/plan.rs
git commit -m "feat(cli): plan writer — marker-fenced .oharaignore rendering + safe merge"
```

---

### Task D.5 — Wire walker → aggregator → renderer in `run()`

**Files:**
- Modify: `crates/ohara-cli/src/commands/plan.rs`

- [ ] **Step 1: Implement the orchestration**

Replace the stub `pub async fn run(_args: Args)` with the wired
implementation:

```rust
use ohara_git::GitWalker;
use std::io::Write;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn run(args: Args) -> Result<()> {
    let canonical = std::fs::canonicalize(&args.path)
        .with_context(|| format!("canonicalize {}", args.path.display()))?;

    println!("walking commit history (paths only)…");
    let walker = GitWalker::open(&canonical).context("open git repo")?;

    let start = std::time::Instant::now();
    let mut agg = HotmapAggregator::default();
    walker.for_each_commit_paths(|_meta, paths| {
        let owned: Vec<String> = paths.iter().cloned().collect();
        agg.record(&owned);
        Ok(())
    })?;
    let elapsed = start.elapsed();
    println!(
        "walked {} commits in {:.1}s",
        agg.total_commits(),
        elapsed.as_secs_f64()
    );

    print_hotmap(&agg);
    let suggestions = suggest_patterns(&agg);
    print_suggestions(&suggestions, agg.total_commits());
    print_gpu_hint();

    if args.no_write {
        return Ok(());
    }

    let target = canonical.join(".oharaignore");
    let new_section = render_oharaignore_body(&suggestions, VERSION);

    let final_text = if args.replace || !target.exists() {
        new_section
    } else {
        let existing = std::fs::read_to_string(&target)
            .with_context(|| format!("read {}", target.display()))?;
        merge_oharaignore(&existing, &new_section)?
    };

    if !args.yes {
        print!("write {}? [y/N] ", target.display());
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).context("stdin read")?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("aborted; no file written");
            return Ok(());
        }
    }

    std::fs::write(&target, final_text)
        .with_context(|| format!("write {}", target.display()))?;
    println!("wrote {}", target.display());
    Ok(())
}

/// Print the top-N directories by commit share.
fn print_hotmap(agg: &HotmapAggregator) {
    let total = agg.total_commits().max(1);
    let mut top: Vec<(&String, &u64)> = agg
        .counts()
        .iter()
        .filter(|(k, _)| {
            let slash_count = k.matches('/').count();
            slash_count == 1 && k.ends_with('/')
        })
        .collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!("\ntop-level directories by commit share:");
    for (k, count) in top.iter().take(20) {
        let share = (**count as f64 / total as f64) * 100.0;
        println!("  {:<40} {:>7} ({:>4.1}%)", k, count, share);
    }
}

fn print_suggestions(suggestions: &[String], total: u64) {
    println!("\nproposed auto-generated section:");
    if suggestions.is_empty() {
        println!("  (no high-share top-level directories — nothing suggested)");
    }
    for s in suggestions {
        println!("  {s}");
    }
    println!("\ntotal commits surveyed: {total}");
}

fn print_gpu_hint() {
    let coreml = cfg!(feature = "coreml");
    let cuda = cfg!(feature = "cuda");
    if coreml || cuda {
        println!(
            "\nnote: ohara is built with --features {} ; embedding will use the accelerator.",
            if coreml { "coreml" } else { "cuda" }
        );
    } else {
        println!(
            "\nnote: rebuild with --features coreml (Apple) or --features cuda (NVIDIA) for ~3-5x embed speedup."
        );
    }
}
```

- [ ] **Step 2: Verify the file compiles**

Run: `cargo build -p ohara-cli`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/src/commands/plan.rs
git commit -m "feat(cli): wire plan command — walker → aggregator → renderer → writer"
```

---

### Task D.6 — Register `Plan` subcommand in `main.rs`

**Files:**
- Modify: `crates/ohara-cli/src/main.rs`

- [ ] **Step 1: Add the subcommand**

In `crates/ohara-cli/src/main.rs`, add to the `Cmd` enum (alphabetical
slot, after `Init`):

```rust
    /// Survey a repo's history and write a `.oharaignore` with
    /// suggested skip patterns. Plan-26.
    Plan(commands::plan::Args),
```

In the `match cli.command` block:

```rust
        Cmd::Plan(a) => commands::plan::run(a).await,
```

- [ ] **Step 2: Smoke test**

Run: `cargo run -p ohara-cli -- plan --help`
Expected: clap renders help text including `--yes`, `--no-write`,
`--replace`, and the `[PATH]` positional defaulting to `.`.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/src/main.rs
git commit -m "feat(cli): register `ohara plan` subcommand"
```

---

## Phase E — Status surface + docs

### Task E.1 — `ignore_rules` line in `ohara status`

**Files:**
- Modify: `crates/ohara-cli/src/commands/status.rs`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `crates/ohara-cli/src/commands/status.rs`:

```rust
    #[test]
    fn render_ignore_summary_counts_by_layer() {
        // Plan 26 Task E.1: a one-line summary of the active filter.
        let s = render_ignore_summary(/* builtins */ 18, /* gitattrs */ 0, /* user */ 5);
        assert_eq!(s, "ignore_rules: 23 patterns (18 built-in + 0 gitattributes + 5 user)");
    }

    #[test]
    fn render_ignore_summary_zero_user_no_gitattrs_still_prints() {
        let s = render_ignore_summary(18, 0, 0);
        assert!(s.contains("18 patterns"), "got: {s}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-cli status::tests::render_ignore_summary`
Expected: FAIL with "no function `render_ignore_summary`".

- [ ] **Step 3: Implement + plumb**

Append to `crates/ohara-cli/src/commands/status.rs` (above the existing
`tests` module):

```rust
/// Render the one-line summary printed by `ohara status` when the
/// repo has any ignore rules active. Pulled out for unit testing.
pub fn render_ignore_summary(builtins: usize, gitattrs: usize, user: usize) -> String {
    let total = builtins + gitattrs + user;
    format!(
        "ignore_rules: {total} patterns ({builtins} built-in + {gitattrs} gitattributes + {user} user)"
    )
}

fn count_ignore_layers(repo_root: &std::path::Path) -> (usize, usize, usize) {
    let builtins = ohara_core::BUILT_IN_DEFAULTS.len();
    let gitattrs = std::fs::read_to_string(repo_root.join(".gitattributes"))
        .map(|s| s.lines().filter(|l| l.contains("linguist-")).count())
        .unwrap_or(0);
    let user = std::fs::read_to_string(repo_root.join(".oharaignore"))
        .map(|s| {
            s.lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .count()
        })
        .unwrap_or(0);
    (builtins, gitattrs, user)
}
```

In the `run` function, after the existing `println!` block (around line
60-68 of the current file), add:

```rust
    let (b, g, u) = count_ignore_layers(&canonical);
    println!("{}", render_ignore_summary(b, g, u));
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ohara-cli status::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/status.rs
git commit -m "feat(cli): `ohara status` prints ignore_rules summary"
```

---

### Task E.2 — Docs: indexing.md section + README quickstart

**Files:**
- Modify: `docs-book/src/architecture/indexing.md`
- Modify: `README.md`

- [ ] **Step 1: Add a `.oharaignore` section to `indexing.md`**

Append (or insert under a top-level heading near the Indexer
description):

```markdown
## Path-aware indexing — `.oharaignore`

`ohara` consults a layered ignore filter at index time. Three sources
are merged, with the last winning so `!negate` patterns work:

1. **Built-in defaults** (compiled into `ohara-core`) — lockfiles,
   `node_modules/`, `target/`, `vendor/`, `dist/`, etc.
2. **`.gitattributes`** — paths flagged `linguist-generated=true` or
   `linguist-vendored=true`.
3. **`.oharaignore`** at repo root — gitignore-syntax, team-shared.

Run `ohara plan` to survey a repo's commit-share hotmap and write a
suggested `.oharaignore`. The planner runs a paths-only libgit2 walk
(seconds-to-minutes even on giant repos), groups commits by top-level
directory, and proposes ignoring high-share directories outside a
small documentation allowlist.

When a commit's changed paths are 100% ignored, the indexer skips it
entirely (no rows written) but advances `last_indexed_commit` past it,
so `--incremental` runs work normally.
```

- [ ] **Step 2: Add `ohara plan` to the README quickstart**

In `README.md`, find the existing "quickstart" or `ohara init` section
and add:

```markdown
For large repos (Linux-class, 100k+ commits), run `ohara plan` first to
write a `.oharaignore` that drops mechanical noise (vendored deps,
generated code, lockfiles). This is also where the `--features coreml`
(Apple) / `--features cuda` (NVIDIA Linux) builds pay off — embedding
on the accelerator is 3-5× faster.
```

- [ ] **Step 3: Commit**

```bash
git add docs-book/src/architecture/indexing.md README.md
git commit -m "docs(plan-26): document .oharaignore + ohara plan in indexing.md and README"
```

---

## Phase F — End-to-end integration tests

### Task F.1 — Fixture: `vendor/` ignored, mixed commit indexes only real paths

**Files:**
- Create: `crates/ohara-cli/tests/plan_26_ignore_e2e.rs`

- [ ] **Step 1: Write the failing test**

Create the file:

```rust
//! Plan-26 end-to-end: `.oharaignore` causes the indexer to skip
//! ignored paths while keeping real source hunks.

use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
fn mixed_commit_with_vendor_ignored_indexes_only_real_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    // Build a minimal repo: one commit touching `src/main.rs` + `vendor/foo.c`.
    git_init(repo);
    write(repo.join("src"), "main.rs", "fn main() {}\n");
    write(repo.join("vendor"), "foo.c", "int main(void) { return 0; }\n");
    git_add_all(repo);
    git_commit(repo, "feat: add main + vendor stub");

    // Write `.oharaignore`.
    std::fs::write(repo.join(".oharaignore"), "vendor/\n").unwrap();

    // Run `ohara index` against the fixture.
    let out = Command::new(ohara_bin())
        .arg("index")
        .arg(repo)
        .output()
        .expect("run ohara index");
    assert!(out.status.success(), "ohara index failed: {out:?}");

    // Run `ohara query` for a vendor-specific token; expect 0 hits.
    let q = Command::new(ohara_bin())
        .args(["query", "--query", "return 0"])
        .arg(repo)
        .output()
        .expect("run ohara query");
    let stdout = String::from_utf8_lossy(&q.stdout);
    assert!(
        !stdout.contains("vendor/foo.c"),
        "vendor path leaked into query results: {stdout}"
    );
}

fn git_init(p: &std::path::Path) {
    Command::new("git").arg("init").arg(p).output().unwrap();
}
fn git_add_all(p: &std::path::Path) {
    Command::new("git").arg("-C").arg(p).args(["add", "."]).output().unwrap();
}
fn git_commit(p: &std::path::Path, msg: &str) {
    Command::new("git").arg("-C").arg(p)
        .args(["-c", "user.email=a@a", "-c", "user.name=a", "commit", "-m", msg])
        .output().unwrap();
}
fn write(dir: std::path::PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ohara-cli --test plan_26_ignore_e2e`
Expected: PASS (Phase A-C wiring should already make this work).

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/tests/plan_26_ignore_e2e.rs
git commit -m "test(cli): plan-26 e2e — mixed commit, vendor path filtered"
```

---

### Task F.2 — Fixture: 100%-vendor commit drops to zero rows but watermark advances

**Files:**
- Modify: `crates/ohara-cli/tests/plan_26_ignore_e2e.rs`

- [ ] **Step 1: Append the second test**

Add to `plan_26_ignore_e2e.rs`:

```rust
#[test]
fn pure_vendor_commit_advances_watermark_with_zero_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git_init(repo);
    // Two commits: one real, one 100% vendor.
    write(repo.join("src"), "main.rs", "fn main() {}\n");
    git_add_all(repo);
    git_commit(repo, "feat: add main");
    write(repo.join("vendor"), "deps.lock", "v1\n");
    git_add_all(repo);
    git_commit(repo, "chore(deps): bump");

    std::fs::write(repo.join(".oharaignore"), "vendor/\n").unwrap();

    let head = String::from_utf8(
        Command::new("git")
            .arg("-C").arg(repo)
            .args(["rev-parse", "HEAD"])
            .output().unwrap()
            .stdout,
    ).unwrap().trim().to_string();

    let idx = Command::new(ohara_bin()).arg("index").arg(repo).output().unwrap();
    assert!(idx.status.success());

    let st = Command::new(ohara_bin()).arg("status").arg(repo).output().unwrap();
    let stdout = String::from_utf8_lossy(&st.stdout);

    assert!(
        stdout.contains(&format!("last_indexed_commit: {head}")),
        "watermark did not advance to HEAD; status:\n{stdout}"
    );
    assert!(
        stdout.contains("commits_behind_head: 0"),
        "commits_behind_head should be 0; status:\n{stdout}"
    );
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p ohara-cli --test plan_26_ignore_e2e pure_vendor_commit_advances_watermark`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/tests/plan_26_ignore_e2e.rs
git commit -m "test(cli): plan-26 e2e — 100% vendor commit advances watermark"
```

---

### Task F.3 — Regression: no `.oharaignore` produces unchanged behaviour

**Files:**
- Modify: `crates/ohara-cli/tests/plan_26_ignore_e2e.rs`

- [ ] **Step 1: Append the regression test**

```rust
#[test]
fn no_oharaignore_indexes_all_paths_unchanged() {
    // Plan 26 regression: when no `.oharaignore` exists at the repo
    // root, the indexer's behaviour matches today's (every changed
    // file is indexed). Built-in defaults still apply, but the user
    // has opted into nothing extra.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git_init(repo);
    write(repo.join("src"), "main.rs", "fn main() {}\n");
    write(repo.join("custom_lib"), "thing.rs", "// keep me\n");
    git_add_all(repo);
    git_commit(repo, "feat: initial");

    let idx = Command::new(ohara_bin()).arg("index").arg(repo).output().unwrap();
    assert!(idx.status.success());

    // The non-builtin path `custom_lib/` must still appear when we
    // query for it (no .oharaignore = no extra filtering).
    let q = Command::new(ohara_bin())
        .args(["query", "--query", "keep me"])
        .arg(repo)
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&q.stdout);
    assert!(
        stdout.contains("custom_lib/thing.rs"),
        "expected custom_lib hit without .oharaignore; got:\n{stdout}"
    );
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p ohara-cli --test plan_26_ignore_e2e no_oharaignore_indexes_all_paths_unchanged
git add crates/ohara-cli/tests/plan_26_ignore_e2e.rs
git commit -m "test(cli): plan-26 regression — no .oharaignore preserves prior behaviour"
```

---

### Task F.4 — `ohara plan` end-to-end on a small fixture

**Files:**
- Create: `crates/ohara-cli/tests/plan_26_plan_command_e2e.rs`

- [ ] **Step 1: Write the test**

```rust
//! Plan-26 end-to-end: `ohara plan --yes` produces a `.oharaignore`
//! at the repo root with the auto-generated marker block.

use std::process::Command;

fn ohara_bin() -> String { env!("CARGO_BIN_EXE_ohara").to_string() }

#[test]
fn plan_yes_writes_marker_fenced_oharaignore() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    // Synthesize a repo where one top-level dir dominates commit count.
    Command::new("git").arg("init").arg(repo).output().unwrap();
    for i in 0..10 {
        let p = repo.join("noise");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(format!("f{i}.txt")), "x").unwrap();
        Command::new("git").arg("-C").arg(repo).args(["add", "."]).output().unwrap();
        Command::new("git").arg("-C").arg(repo)
            .args(["-c", "user.email=a@a", "-c", "user.name=a",
                   "commit", "-m", &format!("noise {i}")])
            .output().unwrap();
    }
    std::fs::write(repo.join("README.md"), "real\n").unwrap();
    Command::new("git").arg("-C").arg(repo).args(["add", "."]).output().unwrap();
    Command::new("git").arg("-C").arg(repo)
        .args(["-c", "user.email=a@a", "-c", "user.name=a",
               "commit", "-m", "real"])
        .output().unwrap();

    let out = Command::new(ohara_bin())
        .args(["plan", "--yes"])
        .arg(repo)
        .output()
        .expect("run ohara plan");
    assert!(out.status.success(), "plan failed: {out:?}");

    let body = std::fs::read_to_string(repo.join(".oharaignore"))
        .expect(".oharaignore must exist");
    assert!(body.contains("ohara plan v"), "begin marker missing");
    assert!(body.contains("end auto-generated"), "end marker missing");
    assert!(body.contains("noise/"), "high-share dir not suggested: {body}");
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p ohara-cli --test plan_26_plan_command_e2e`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/tests/plan_26_plan_command_e2e.rs
git commit -m "test(cli): plan-26 e2e — `ohara plan --yes` writes marker-fenced file"
```

---

## Pre-completion checklist

Before opening the PR (per `CONTRIBUTING.md` §13):

- [ ] `cargo fmt --all` clean.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --workspace` green.
- [ ] No file > 500 lines (especially `crates/ohara-cli/src/commands/plan.rs`
      and `crates/ohara-core/src/ignore.rs`). Split by responsibility if so.
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code (the
      `.expect("invariant: …")` form on `BUILT_IN_DEFAULTS` build is OK
      because it documents a true invariant).
- [ ] No `println!` / `eprintln!` outside `ohara-cli` user-facing
      output (the planner's hotmap printing is in `ohara-cli` and OK).
- [ ] Newtypes preserved at API boundaries (`RepoPath` etc. — review
      whether `String` paths in `IgnoreFilter::is_ignored` should be
      `&RepoPath` instead; if `RepoPath` is the project's standard,
      tighten the trait signature before merge).
- [ ] Workspace-only deps: `ignore` and `tempfile` (if added) live in
      root `Cargo.toml` `[workspace.dependencies]`.

## Out of scope (companion plans)

- **Plan 27 (Spec B) — chunk-level content dedup.** Extends `ContentHash`
  to chunks; reuses stored vectors when `(content_hash, embed_model)`
  hits. Independent of this plan.
- **Plan 28 (Spec D) — parallel commit pipeline.** Worker pool around
  parse/embed; in-order watermark serializer. Sequenced *after* this
  plan and plan 27 so the chunker + pre-embed stages are stable before
  the orchestrator is parallelised.
