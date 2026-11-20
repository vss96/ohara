use anyhow::Result;
use std::path::Path;

pub mod daemon;
pub mod explain;
pub mod format;
pub mod index;
pub mod index_wizard;
pub mod init;
pub mod plan;
pub mod plan_summary;
pub mod provider;
pub mod query;
pub mod serve;
pub mod status;
pub mod update;

/// Re-export of `ohara_core::paths::ohara_home`. CLI callers expect an
/// `anyhow::Result`, so we map through `?` at use sites.
pub use ohara_core::paths::{index_db_path, ohara_home};

/// Resolve the repo identity (shared id, worktree path, worktree key) of the
/// repository containing `repo_path`. Linked worktrees map to the main
/// clone's id.
pub fn resolve_repo_id<P: AsRef<Path>>(repo_path: P) -> Result<ohara_git::RepoIdentity> {
    ohara_git::RepoIdentity::resolve(repo_path.as_ref())
}
