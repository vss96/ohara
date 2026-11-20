//! `--incremental` fast path and tip bookkeeping helpers for `ohara index`.

use anyhow::{anyhow, Result};
use ohara_core::{head_symbols_stale, main_needs_first_run, CommitSha, CommitsBehind, TipStore};
use ohara_git::RepoIdentity;

/// HEAD of the worktree as a validated [`CommitSha`].
pub(super) fn head_sha(id: &RepoIdentity) -> Result<CommitSha> {
    ohara_git::read_head(&id.worktree_path)?
        .ok_or_else(|| anyhow!("HEAD has no commits yet; nothing to index"))
}

/// True when every commit reachable from HEAD is already covered by an
/// indexed tip of any worktree, i.e. there is nothing to walk. Never true
/// for the main checkout before it has its own tip: that run records the
/// tip and takes over the HEAD-symbol snapshot. Never true for the
/// snapshot's owner while the snapshot's recorded provenance is not HEAD
/// (the snapshot would stay at another commit).
pub(super) async fn nothing_to_walk(tips: &dyn TipStore, id: &RepoIdentity) -> Result<bool> {
    let all = tips.indexed_tips(&id.repo_id).await?;
    if main_needs_first_run(&id.worktree, &all) {
        return Ok(false);
    }
    // One HEAD observation backs the whole decision: `count_hiding` walks
    // whatever HEAD is when it runs, so a HEAD that moved since the
    // snapshot check (e.g. reset to an ancestor the tips hide) must not be
    // reported as up to date. Index instead.
    let head = head_sha(id)?;
    let snapshot = tips.head_symbol_snapshot(&id.repo_id).await?;
    if head_symbols_stale(&id.worktree, &all, snapshot.as_ref(), &head) {
        return Ok(false);
    }
    let hide: Vec<String> = all.iter().map(|t| t.sha.as_str().to_string()).collect();
    let behind = ohara_git::GitCommitsBehind::open(&id.worktree_path)?;
    if behind.count_hiding(&hide).await? != 0 {
        return Ok(false);
    }
    Ok(head_sha(id)? == head)
}
