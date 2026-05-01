use anyhow::{anyhow, Result};
use ohara_core::types::RepoId;
use std::path::{Path, PathBuf};

pub mod index;
pub mod init;
pub mod query;
pub mod status;

/// Re-export of `ohara_core::paths::ohara_home`. CLI callers expect an
/// `anyhow::Result`, so we map through `?` at use sites.
pub use ohara_core::paths::{index_db_path, ohara_home};

pub fn resolve_repo_id<P: AsRef<Path>>(repo_path: P) -> Result<(RepoId, PathBuf, String)> {
    let canonical = std::fs::canonicalize(repo_path.as_ref())
        .map_err(|e| anyhow!("canonicalize {}: {e}", repo_path.as_ref().display()))?;
    let walker = ohara_git::GitWalker::open(&canonical).map_err(|e| anyhow!("open repo: {e}"))?;
    let first = walker.first_commit_sha().map_err(|e| anyhow!("first commit: {e}"))?;
    let canonical_str = canonical.to_string_lossy().to_string();
    let id = RepoId::from_parts(&first, &canonical_str);
    Ok((id, canonical, first))
}
