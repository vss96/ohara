//! `--incremental` fast path and tip bookkeeping helpers for `ohara index`.

use anyhow::Result;
use ohara_core::{CommitSha, CommitsBehind, TipStore};
use ohara_git::RepoIdentity;

/// HEAD of the worktree as a validated [`CommitSha`].
pub(super) fn head_sha(id: &RepoIdentity) -> Result<CommitSha> {
    let head = ohara_git::GitWalker::open(&id.worktree_path)?.head_commit_sha()?;
    Ok(CommitSha::parse(&head)?)
}

/// True when every commit reachable from HEAD is already covered by an
/// indexed tip of any worktree, i.e. there is nothing to walk.
pub(super) async fn nothing_to_walk(tips: &dyn TipStore, id: &RepoIdentity) -> Result<bool> {
    let hide: Vec<String> = tips
        .indexed_tips(&id.repo_id)
        .await?
        .iter()
        .map(|t| t.sha.as_str().to_string())
        .collect();
    let behind = ohara_git::GitCommitsBehind::open(&id.worktree_path)?;
    Ok(behind.count_hiding(&hide).await? == 0)
}
