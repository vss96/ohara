//! Per-worktree tip bookkeeping for `Indexer::run` (plan-33).

use crate::types::RepoId;
use crate::worktree::{CommitSha, TipStore, WorktreeKey};
use crate::Result;
use std::sync::Arc;

/// Tells the indexer which worktree it is indexing and where the shared
/// store's per-worktree tips live. When set, the walk hides every
/// recorded tip instead of reading the single legacy watermark.
#[derive(Clone)]
pub struct TipContext {
    /// Store of per-worktree indexed tips.
    pub tips: Arc<dyn TipStore>,
    /// The worktree being indexed.
    pub worktree: WorktreeKey,
    /// The worktree's HEAD when the run started.
    pub head: CommitSha,
}

impl TipContext {
    /// Every indexed tip of every worktree: the commits the walk must hide.
    pub(crate) async fn hidden(&self, repo_id: &RepoId) -> Result<Vec<CommitSha>> {
        let tips = self.tips.indexed_tips(repo_id).await?;
        Ok(tips.into_iter().map(|t| t.sha).collect())
    }

    /// Record `head` as this worktree's tip after a clean run. A run with
    /// failed commits leaves the tip untouched so the next run retries.
    pub(crate) async fn record(&self, repo_id: &RepoId, commits_failed: usize) -> Result<()> {
        if commits_failed > 0 {
            return Ok(());
        }
        self.tips
            .set_indexed_tip(repo_id, &self.worktree, &self.head)
            .await
    }

    /// Only the main checkout owns the HEAD-symbol snapshot.
    pub(crate) fn owns_head_symbols(&self) -> bool {
        self.worktree.is_main()
    }
}
