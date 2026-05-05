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
