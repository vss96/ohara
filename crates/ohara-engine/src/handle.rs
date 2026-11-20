//! Per-repo and per-worktree handles cached by `RetrievalEngine` (plan-33).

use ohara_core::types::RepoId;
use ohara_core::{Retriever, Storage, TipStore, WorktreeKey};
use ohara_git::Blamer;
use std::path::PathBuf;
use std::sync::Arc;

/// Shared by every worktree of one repository: the one SQLite index and
/// the retriever built over it.
pub struct RepoStore {
    /// Id shared by the main clone and all of its linked worktrees.
    pub repo_id: RepoId,
    /// The repository's index.
    pub storage: Arc<dyn Storage>,
    /// Per-worktree indexed tips (same underlying store as `storage`).
    pub tips: Arc<dyn TipStore>,
    /// Retrieval pipeline over `storage`.
    pub retriever: Retriever,
}

/// One per worktree path: the worktree's checkout plus the shared store.
pub struct RepoHandle {
    /// Same as `store.repo_id`.
    pub repo_id: RepoId,
    /// Canonical path of this worktree.
    pub repo_path: PathBuf,
    /// Which worktree of the repository this handle serves.
    pub worktree: WorktreeKey,
    /// The repository-wide store shared with sibling worktrees.
    pub store: Arc<RepoStore>,
    /// Blamer opened on this worktree's checkout.
    pub blamer: Arc<Blamer>,
}
