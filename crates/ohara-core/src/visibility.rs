//! Per-worktree commit visibility.
//!
//! One index serves a clone and all its linked worktrees; a query from a
//! worktree may only return commits reachable from that worktree's HEAD.
//! `ohara-core` defines the contract; a git-backed implementation lives in
//! a leaf crate.

use crate::{CommitSha, Result};
use async_trait::async_trait;

/// Answers "is this commit reachable from `head`?" for a batch of commits.
#[async_trait]
pub trait CommitVisibility: Send + Sync {
    /// One bool per entry of `commits`: true when it is `head` or an ancestor of `head`.
    /// A commit whose object is missing is not visible.
    async fn visible(&self, head: &CommitSha, commits: &[CommitSha]) -> Result<Vec<bool>>;
}

/// The HEAD to scope a query to, plus the oracle that judges reachability.
pub struct VisibilityScope<'a> {
    /// Commit the querying worktree has checked out.
    pub head: &'a CommitSha,
    /// Reachability oracle.
    pub oracle: &'a dyn CommitVisibility,
}
