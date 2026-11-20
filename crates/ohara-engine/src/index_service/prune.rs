//! Prune for the shared store (plan-33): delete commits no ref or
//! worktree HEAD reaches and tidy the indexed tips.

use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::{CommitSha, IndexedTip, WorktreeKey};
use std::collections::HashSet;
use std::path::PathBuf;

/// Commits deleted per `TipStore::delete_commits` call (one transaction each).
const PRUNE_BATCH: usize = 500;

/// Prune body; returns `(commits_deleted, tips_removed)`. Caller holds
/// the repo's job lock and refreshes caches whatever the result.
pub(super) async fn prune_locked(handle: &RepoHandle) -> crate::Result<(u64, u64)> {
    let tips = &handle.store.tips;
    // List indexed commits BEFORE snapshotting reachability. Another process
    // (e.g. the CLI post-commit hook) may index a new commit meanwhile; with
    // this order that commit is never a candidate. The reverse order would
    // see it indexed but missing from a stale reachable set, and delete it.
    let indexed = tips
        .indexed_commit_shas(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    let (reachable, live_worktrees) = git_liveness(handle.repo_path.clone()).await?;
    let doomed: Vec<CommitSha> = indexed
        .into_iter()
        .filter(|c| !reachable.contains(c.as_str()))
        .collect();
    let mut commits_deleted = 0u64;
    for batch in doomed.chunks(PRUNE_BATCH) {
        commits_deleted += tips
            .delete_commits(&handle.repo_id, batch)
            .await
            .map_err(EngineError::from)?;
    }
    let mut tips_removed = 0u64;
    let all = tips
        .indexed_tips(&handle.repo_id)
        .await
        .map_err(EngineError::from)?;
    for tip in all {
        let fate = tip_fate(&tip, &reachable, &live_worktrees);
        if fate == TipFate::Keep {
            continue;
        }
        if fate == TipFate::Retire {
            let key = WorktreeKey::retired(&tip.worktree, &tip.sha);
            tips.set_indexed_tip(&handle.repo_id, &key, &tip.sha)
                .await
                .map_err(EngineError::from)?;
        }
        tips.remove_indexed_tip(&handle.repo_id, &tip.worktree)
            .await
            .map_err(EngineError::from)?;
        tips_removed += 1;
    }
    Ok((commits_deleted, tips_removed))
}

/// What prune does with one indexed tip.
#[derive(Debug, PartialEq, Eq)]
enum TipFate {
    Keep,
    /// Remove it.
    Drop,
    /// Its worktree is gone but a ref still reaches its commits: keep it
    /// as a retired tip so those commits stay behind the visibility filter.
    Retire,
}

/// The main tip always stays. A live worktree's tip stays. A retired tip
/// stays while some ref or worktree HEAD reaches it. A gone worktree's tip
/// is retired while reachable, else dropped.
fn tip_fate(tip: &IndexedTip, reachable: &HashSet<String>, live: &HashSet<String>) -> TipFate {
    let reached = reachable.contains(tip.sha.as_str());
    if tip.worktree.is_main() {
        return TipFate::Keep;
    }
    if tip.worktree.is_retired() {
        return match reached {
            true => TipFate::Keep,
            false => TipFate::Drop,
        };
    }
    if live.contains(tip.worktree.as_str()) {
        return TipFate::Keep;
    }
    match reached {
        true => TipFate::Retire,
        false => TipFate::Drop,
    }
}

/// Commits reachable from any ref or worktree HEAD, and the names of the
/// linked worktrees git still knows about. Fails closed on any git error.
async fn git_liveness(path: PathBuf) -> crate::Result<(HashSet<String>, HashSet<String>)> {
    tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let reachable = ohara_git::reachable_commit_shas(&path)
            .map_err(|e| EngineError::Git(format!("reachable commits: {e:#}")))?;
        let repo = git2::Repository::discover(&path)
            .map_err(|e| EngineError::Git(format!("open {}: {e}", path.display())))?;
        let names = repo
            .worktrees()
            .map_err(|e| EngineError::Git(format!("list worktrees: {e}")))?;
        let live = names.iter().flatten().map(str::to_string).collect();
        Ok((reachable, live))
    })
    .await
    .map_err(|e| EngineError::Internal(format!("prune git task: {e}")))?
}
