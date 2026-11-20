//! Per-worktree tip bookkeeping for `Indexer::run` (plan-33).

use crate::types::RepoId;
use crate::visibility::CommitVisibility;
use crate::worktree::{owns_head_symbols, CommitSha, TipStore, WorktreeKey};
use crate::Result;
use std::sync::Arc;

/// Tells the indexer which worktree it is indexing and where the shared
/// store's per-worktree tips live. When set, the walk hides every
/// recorded tip instead of reading the single legacy watermark.
#[derive(Clone)]
pub struct TipContext {
    /// Store of per-worktree indexed tips.
    pub tips: Arc<dyn TipStore>,
    /// Ancestry oracle used to tell a fast-forward from a move to a
    /// non-descendant (branch switch, reset, slot reuse).
    pub visibility: Arc<dyn CommitVisibility>,
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
    ///
    /// When the previous tip is not an ancestor of `head`, it is kept as a
    /// retired tip first: its commits stay indexed, so it must keep counting
    /// as a tip (walk hiding, the "every tip visible" fast path, coverage)
    /// until prune finds it unreachable.
    pub(crate) async fn record(&self, repo_id: &RepoId, commits_failed: usize) -> Result<()> {
        if commits_failed > 0 {
            return Ok(());
        }
        self.retire_previous(repo_id).await?;
        self.tips
            .set_indexed_tip(repo_id, &self.worktree, &self.head)
            .await
    }

    /// Keep the worktree's previous tip under a retired key when `head`
    /// does not descend from it.
    async fn retire_previous(&self, repo_id: &RepoId) -> Result<()> {
        let tips = self.tips.indexed_tips(repo_id).await?;
        let Some(prev) = tips.into_iter().find(|t| t.worktree == self.worktree) else {
            return Ok(());
        };
        if prev.sha == self.head {
            return Ok(());
        }
        let verdict = self
            .visibility
            .visible(&self.head, std::slice::from_ref(&prev.sha))
            .await?;
        if verdict.first().copied().unwrap_or(false) {
            return Ok(());
        }
        tracing::info!(
            worktree = self.worktree.as_str(),
            retired = prev.sha.as_str(),
            "worktree moved off its indexed tip; retiring it"
        );
        let key = WorktreeKey::retired(&self.worktree, &prev.sha);
        self.tips.set_indexed_tip(repo_id, &key, &prev.sha).await
    }

    /// Whether this run writes the shared HEAD-symbol snapshot. See
    /// [`owns_head_symbols`]; a write replaces the whole snapshot, so
    /// main's first run takes over whatever a linked worktree wrote.
    pub(crate) async fn owns_head_symbols(&self, repo_id: &RepoId) -> Result<bool> {
        let tips = self.tips.indexed_tips(repo_id).await?;
        Ok(owns_head_symbols(&self.worktree, &tips))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::CommitVisibility;
    use crate::worktree::IndexedTip;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::Mutex;

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(&c.to_string().repeat(40)).unwrap()
    }

    #[derive(Default)]
    struct MemTips(Mutex<Vec<IndexedTip>>);

    impl MemTips {
        fn with(tips: Vec<(WorktreeKey, CommitSha)>) -> Self {
            let tips = tips
                .into_iter()
                .map(|(worktree, sha)| IndexedTip { worktree, sha })
                .collect();
            Self(Mutex::new(tips))
        }
        fn snapshot(&self) -> Vec<IndexedTip> {
            self.0.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TipStore for MemTips {
        async fn indexed_tips(&self, _: &RepoId) -> Result<Vec<IndexedTip>> {
            Ok(self.snapshot())
        }
        async fn set_indexed_tip(&self, _: &RepoId, w: &WorktreeKey, s: &CommitSha) -> Result<()> {
            let mut tips = self.0.lock().unwrap();
            tips.retain(|t| &t.worktree != w);
            tips.push(IndexedTip {
                worktree: w.clone(),
                sha: s.clone(),
            });
            Ok(())
        }
        async fn remove_indexed_tip(&self, _: &RepoId, _: &WorktreeKey) -> Result<()> {
            Ok(())
        }
        async fn indexed_commit_shas(&self, _: &RepoId) -> Result<Vec<CommitSha>> {
            Ok(vec![])
        }
        async fn delete_commits(&self, _: &RepoId, _: &[CommitSha]) -> Result<u64> {
            Ok(0)
        }
    }

    /// `visible(head, c)` is true for `head` itself and the listed ancestors.
    struct Ancestors(HashSet<String>);

    #[async_trait]
    impl CommitVisibility for Ancestors {
        async fn visible(&self, head: &CommitSha, commits: &[CommitSha]) -> Result<Vec<bool>> {
            Ok(commits
                .iter()
                .map(|c| c == head || self.0.contains(c.as_str()))
                .collect())
        }
    }

    fn ctx(tips: Arc<MemTips>, ancestors: &[char], head: char) -> TipContext {
        let set = ancestors.iter().map(|c| sha(*c).as_str().to_string());
        TipContext {
            tips,
            visibility: Arc::new(Ancestors(set.collect())),
            worktree: WorktreeKey::linked("slot-1").unwrap(),
            head: sha(head),
        }
    }

    fn repo() -> RepoId {
        RepoId::from_parts("first", "/r")
    }

    #[tokio::test]
    async fn moving_to_a_non_descendant_retires_the_old_tip() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(MemTips::with(vec![(slot1.clone(), sha('a'))]));
        ctx(tips.clone(), &[], 'c')
            .record(&repo(), 0)
            .await
            .unwrap();
        let snap = tips.snapshot();
        assert_eq!(snap.len(), 2, "got {snap:?}");
        assert!(snap.contains(&IndexedTip {
            worktree: slot1.clone(),
            sha: sha('c')
        }));
        assert!(snap.contains(&IndexedTip {
            worktree: WorktreeKey::retired(&slot1, &sha('a')),
            sha: sha('a')
        }));
    }

    #[tokio::test]
    async fn fast_forward_does_not_retire() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(MemTips::with(vec![(slot1.clone(), sha('a'))]));
        ctx(tips.clone(), &['a'], 'c')
            .record(&repo(), 0)
            .await
            .unwrap();
        assert_eq!(
            tips.snapshot(),
            vec![IndexedTip {
                worktree: slot1,
                sha: sha('c')
            }]
        );
    }

    #[tokio::test]
    async fn same_tip_and_first_tip_do_not_retire() {
        let slot1 = WorktreeKey::linked("slot-1").unwrap();
        let tips = Arc::new(MemTips::with(vec![(slot1.clone(), sha('c'))]));
        ctx(tips.clone(), &[], 'c')
            .record(&repo(), 0)
            .await
            .unwrap();
        assert_eq!(tips.snapshot().len(), 1);
        let empty = Arc::new(MemTips::default());
        ctx(empty.clone(), &[], 'c')
            .record(&repo(), 0)
            .await
            .unwrap();
        assert_eq!(empty.snapshot().len(), 1);
    }
}
