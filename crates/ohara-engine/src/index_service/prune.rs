//! Prune for the shared store (plan-33): delete commits no ref or
//! worktree HEAD reaches and tidy the indexed tips.

use super::PruneOutcome;
use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::types::RepoId;
use ohara_core::{CommitSha, IndexedTip, TipStore, WorktreeKey};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Commits deleted per `TipStore::delete_commits` call (one transaction each).
const PRUNE_BATCH: usize = 500;

/// The prune job body shared by [`RetrievalEngine::prune_unreachable`] and
/// the auto-prune after an index job. Caller holds the repo's job lock.
pub(super) async fn prune_and_refresh(
    engine: &RetrievalEngine,
    handle: &RepoHandle,
) -> crate::Result<PruneOutcome> {
    let result = prune_locked(
        handle.store.tips.as_ref(),
        &handle.repo_id,
        &handle.repo_path,
    )
    .await;
    // Refresh even on failure: earlier delete batches are already committed.
    engine.refresh_repo_caches(&handle.repo_id);
    let (commits_deleted, tips_removed) = result?;
    tracing::info!(commits_deleted, tips_removed, "pruned unreachable commits");
    Ok(PruneOutcome {
        commits_deleted,
        tips_removed,
    })
}

/// Prune the shared store of the repository containing `repo_path`
/// without a [`RetrievalEngine`]: prune needs only the SQLite index and
/// git, so no embedding or rerank model loads. For `ohara prune` when no
/// daemon is reachable; it takes no job lock, so callers must not run it
/// while a reachable daemon may be writing the same store.
pub async fn prune_unreachable_in_process(repo_path: &Path) -> crate::Result<PruneOutcome> {
    let identity = ohara_git::RepoIdentity::resolve(repo_path)
        .map_err(|e| EngineError::Git(format!("resolve repo identity: {e}")))?;
    let db_path = ohara_core::paths::index_db_path(&identity.repo_id)
        .map_err(|e| EngineError::Internal(format!("index_db_path: {e}")))?;
    if !db_path.is_file() {
        return Err(EngineError::NoIndex {
            path: identity.worktree_path.display().to_string(),
        });
    }
    let storage = ohara_storage::SqliteStorage::open(&db_path)
        .await
        .map_err(|e| EngineError::Storage(format!("open: {e}")))?;
    let (commits_deleted, tips_removed) =
        prune_locked(&storage, &identity.repo_id, &identity.worktree_path).await?;
    tracing::info!(commits_deleted, tips_removed, "pruned unreachable commits");
    Ok(PruneOutcome {
        commits_deleted,
        tips_removed,
    })
}

/// After a successful index job: when the repo has a retired tip (left by
/// a reset, amend or rebase), prune so an unreachable one stops keeping
/// the visibility fast path off. Caller holds the repo's job lock. A
/// failure is logged and never fails the index job. Returns whether a
/// full prune ran.
pub(super) async fn prune_retired_after_index(
    engine: &RetrievalEngine,
    handle: &RepoHandle,
) -> bool {
    let tips = match handle.store.tips.indexed_tips(&handle.repo_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "auto-prune: could not list tips");
            return false;
        }
    };
    if !tips.iter().any(|t| t.worktree.is_retired()) {
        return false;
    }
    if let Err(e) = prune_and_refresh(engine, handle).await {
        tracing::warn!(error = %e, "auto-prune after index failed");
    }
    true
}

/// Prune body over the store of `repo_id`, with git liveness read from
/// `repo_path`; returns `(commits_deleted, tips_removed)`. Callers
/// serialise it with other writers and refresh caches whatever the result.
async fn prune_locked(
    tips: &dyn TipStore,
    repo_id: &RepoId,
    repo_path: &Path,
) -> crate::Result<(u64, u64)> {
    // List indexed commits BEFORE snapshotting reachability. Another process
    // (e.g. the CLI post-commit hook) may index a new commit meanwhile; with
    // this order that commit is never a candidate. The reverse order would
    // see it indexed but missing from a stale reachable set, and delete it.
    let indexed = tips
        .indexed_commit_shas(repo_id)
        .await
        .map_err(EngineError::from)?;
    let (reachable, live_worktrees) = git_liveness(repo_path.to_path_buf()).await?;
    let doomed: Vec<CommitSha> = indexed
        .into_iter()
        .filter(|c| !reachable.contains(c.as_str()))
        .collect();
    let mut commits_deleted = 0u64;
    for batch in doomed.chunks(PRUNE_BATCH) {
        commits_deleted += tips
            .delete_commits(repo_id, batch)
            .await
            .map_err(EngineError::from)?;
    }
    let mut tips_removed = 0u64;
    let all = tips
        .indexed_tips(repo_id)
        .await
        .map_err(EngineError::from)?;
    for tip in all {
        let fate = tip_fate(&tip, &reachable, &live_worktrees);
        if fate == TipFate::Keep {
            continue;
        }
        if fate == TipFate::Retire {
            let key = WorktreeKey::retired(&tip.worktree, &tip.sha);
            tips.set_indexed_tip(repo_id, &key, &tip.sha)
                .await
                .map_err(EngineError::from)?;
        }
        tips.remove_indexed_tip(repo_id, &tip.worktree)
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
