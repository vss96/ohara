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
}
