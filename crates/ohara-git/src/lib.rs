//! git2 wrapper: walk commits, extract per-file diffs.

pub mod walker;
pub use walker::GitWalker;
