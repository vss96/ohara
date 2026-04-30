use anyhow::{anyhow, Result};
use ohara_core::types::RepoId;
use std::path::{Path, PathBuf};

pub mod index;
pub mod query;
pub mod status;

pub fn ohara_home() -> PathBuf {
    if let Ok(s) = std::env::var("OHARA_HOME") {
        return PathBuf::from(s);
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).expect("HOME or USERPROFILE");
    PathBuf::from(home).join(".ohara")
}

pub fn resolve_repo_id<P: AsRef<Path>>(repo_path: P) -> Result<(RepoId, PathBuf, String)> {
    let canonical = std::fs::canonicalize(repo_path.as_ref())
        .map_err(|e| anyhow!("canonicalize {}: {e}", repo_path.as_ref().display()))?;
    let walker = ohara_git::GitWalker::open(&canonical).map_err(|e| anyhow!("open repo: {e}"))?;
    let first = walker.first_commit_sha().map_err(|e| anyhow!("first commit: {e}"))?;
    let canonical_str = canonical.to_string_lossy().to_string();
    let id = RepoId::from_parts(&first, &canonical_str);
    Ok((id, canonical, first))
}

pub fn index_db_path(id: &RepoId) -> PathBuf {
    ohara_home().join(id.as_str()).join("index.sqlite")
}
