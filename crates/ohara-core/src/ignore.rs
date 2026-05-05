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

    /// Load the three-layer filter from a repo root directory.
    ///
    /// Reads `<root>/.gitattributes` and `<root>/.oharaignore` if they
    /// exist; missing files are treated as empty (no error). The
    /// built-in defaults are always applied.
    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let gitattributes = read_to_string_or_empty(&repo_root.join(".gitattributes"))?;
        let user = read_to_string_or_empty(&repo_root.join(".oharaignore"))?;
        Ok(Self::from_strings(BUILT_IN_DEFAULTS, &gitattributes, &user))
    }

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

impl IgnoreFilter for LayeredIgnore {
    fn is_ignored(&self, path: &str) -> bool {
        // User `.oharaignore` wins over earlier layers (so `!negate` works).
        // Any matcher's `Whitelist` (i.e., `!pattern`) short-circuits to
        // "not ignored"; any `Ignore` to "ignored". `None` falls through.
        let p = Path::new(path);
        for layer in [&self.user, &self.gitattributes, &self.builtins] {
            let m = layer.matched_path_or_any_parents(p, false);
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
fn build_gitignore_from_patterns(
    root: &Path,
    patterns: &[&str],
) -> Result<Gitignore, ignore::Error> {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        b.add_line(None, p)?;
    }
    b.build()
}

/// Parse a `.gitattributes` string and emit a `Gitignore` matcher
/// covering paths flagged `linguist-generated=true` or
/// `linguist-vendored=true`. Lines without those attributes are
/// ignored. Patterns are reused verbatim — gitattributes path patterns
/// are gitignore-compatible.
///
/// Malformed individual patterns are logged at warn level and skipped
/// (the rest of the layer still works); a `build()` failure (rare —
/// affects the whole layer) falls back to an empty matcher with a
/// warning so the user notices their `.gitattributes` was ignored.
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
        if let Err(e) = b.add_line(None, pattern) {
            tracing::warn!(
                pattern,
                error = %e,
                "skipped malformed .gitattributes linguist-* pattern"
            );
        }
    }
    b.build().unwrap_or_else(|e| {
        tracing::warn!(error = %e, ".gitattributes ignore layer failed to build; treating as empty");
        Gitignore::empty()
    })
}

/// Parse a `.oharaignore` (gitignore-syntax) string into a matcher.
///
/// Malformed individual patterns are logged at warn level and skipped
/// (the rest of the file still works); a `build()` failure (rare)
/// falls back to an empty matcher with a warning so a typo in the
/// user's file doesn't silently disable the entire layer.
fn build_gitignore_from_lines(root: &Path, contents: &str) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Err(e) = b.add_line(None, line) {
            tracing::warn!(
                pattern = line,
                error = %e,
                "skipped malformed .oharaignore line"
            );
        }
    }
    b.build().unwrap_or_else(|e| {
        tracing::warn!(error = %e, ".oharaignore ignore layer failed to build; treating as empty");
        Gitignore::empty()
    })
}

fn read_to_string_or_empty(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
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
        std::fs::write(dir.path().join(".oharaignore"), "drivers/\n").expect("write .oharaignore");
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
}
