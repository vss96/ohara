//! Prune for the shared store (plan-33): delete commits no ref or
//! worktree HEAD reaches and tidy the indexed tips.

use super::PruneOutcome;
use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::types::RepoId;
use ohara_core::{CommitSha, IndexedTip, TipStore, WorktreeKey};
use std::collections::{HashMap, HashSet};
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

/// After a successful index job: when some retired tip (left by a reset,
/// amend, rebase or branch switch) has become unreachable, prune so it
/// stops keeping the visibility fast path off. Checked after every pass,
/// so a branch deleted later is noticed by the next job. Caller holds the repo's job lock. A
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
    let retired: Vec<CommitSha> = tips
        .into_iter()
        .filter(|t| t.worktree.is_retired())
        .map(|t| t.sha)
        .collect();
    if retired.is_empty() {
        return false;
    }
    // A retired tip a ref still reaches (e.g. after a branch switch) can't
    // be dropped, so a full prune would free nothing; only prune once one
    // has become unreachable. Unknown counts as unreachable: prune itself
    // fails closed.
    if !retired_tip_unreachable(handle.repo_path.clone(), retired).await {
        tracing::debug!("auto-prune: every retired tip is still reachable; skipping");
        return false;
    }
    if let Err(e) = prune_and_refresh(engine, handle).await {
        tracing::warn!(error = %e, "auto-prune after index failed");
    }
    true
}

/// Whether some of `retired` is reached by no ref or worktree HEAD.
/// Errors are logged and count as `true`.
async fn retired_tip_unreachable(path: PathBuf, retired: Vec<CommitSha>) -> bool {
    let res =
        tokio::task::spawn_blocking(move || ohara_git::any_unreachable(&path, &retired)).await;
    match res {
        Ok(Ok(unreachable)) => unreachable,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "auto-prune: reachability check failed; pruning");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto-prune: reachability task failed; pruning");
            true
        }
    }
}

/// Prune body over the store of `repo_id`, with git liveness read from
/// `repo_path`; returns `(commits_deleted, tips_removed)`. Callers
/// serialise it with other writers and refresh caches whatever the result.
///
/// Tips are settled before any commit is deleted, so no tip is left on a
/// deleted commit claiming coverage it no longer has. Tip edits are
/// compare-and-swap on the snapshot, so a tip another writer moved
/// meanwhile is left alone.
async fn prune_locked(
    tips: &dyn TipStore,
    repo_id: &RepoId,
    repo_path: &Path,
) -> crate::Result<(u64, u64)> {
    // Snapshot indexed commits and tips BEFORE reading reachability.
    // Another process (e.g. the CLI post-commit hook) may index a new
    // commit or record a new tip meanwhile; with this order neither is a
    // candidate. The reverse order would see them indexed but missing from
    // a stale reachable set, and delete them.
    let indexed = tips
        .indexed_commit_shas(repo_id)
        .await
        .map_err(EngineError::from)?;
    let snapshot = tips
        .indexed_tips(repo_id)
        .await
        .map_err(EngineError::from)?;
    let liveness = git_liveness(repo_path.to_path_buf(), snapshot.clone()).await?;
    let doomed: Vec<CommitSha> = indexed
        .iter()
        .filter(|c| !liveness.reachable.contains(c.as_str()))
        .cloned()
        .collect();
    let kept: HashSet<&str> = indexed
        .iter()
        .map(CommitSha::as_str)
        .filter(|c| liveness.reachable.contains(*c))
        .collect();
    let tips_removed = settle_tips(tips, repo_id, &snapshot, &liveness, &kept).await?;
    let mut commits_deleted = 0u64;
    for batch in doomed.chunks(PRUNE_BATCH) {
        commits_deleted += tips
            .delete_commits(repo_id, batch)
            .await
            .map_err(EngineError::from)?;
    }
    Ok((commits_deleted, tips_removed))
}

/// Apply [`tip_fate`] to every tip of `snapshot`; returns the tips removed.
async fn settle_tips(
    tips: &dyn TipStore,
    repo_id: &RepoId,
    snapshot: &[IndexedTip],
    liveness: &Liveness,
    kept: &HashSet<&str>,
) -> crate::Result<u64> {
    let mut tips_removed = 0u64;
    for tip in snapshot {
        let fate = tip_fate(tip, liveness, kept);
        let removed = match fate {
            TipFate::Keep => false,
            TipFate::Rewind(to) => {
                let moved = tips
                    .replace_indexed_tip(repo_id, &tip.worktree, &tip.sha, &to)
                    .await
                    .map_err(EngineError::from)?;
                tracing::info!(
                    worktree = tip.worktree.as_str(),
                    from = tip.sha.as_str(),
                    to = to.as_str(),
                    moved,
                    "rewound a tip on a pruned commit"
                );
                false
            }
            TipFate::Retire => {
                let key = WorktreeKey::retired(&tip.worktree, &tip.sha);
                tips.set_indexed_tip(repo_id, &key, &tip.sha)
                    .await
                    .map_err(EngineError::from)?;
                tips.remove_indexed_tip(repo_id, &tip.worktree, &tip.sha)
                    .await
                    .map_err(EngineError::from)?
            }
            TipFate::Drop => tips
                .remove_indexed_tip(repo_id, &tip.worktree, &tip.sha)
                .await
                .map_err(EngineError::from)?,
        };
        tips_removed += u64::from(removed);
    }
    Ok(tips_removed)
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
    /// Its commit is about to be deleted but its checkout lives on: move it
    /// back to the merge base of the old tip and the checkout's HEAD, the
    /// newest commit it still covers, so the next pass re-walks the rest.
    Rewind(CommitSha),
}

/// A reachable tip of the main checkout or a live worktree stays; an
/// unreachable one is rewound to its merge base with that checkout's HEAD
/// when that merge base is indexed and kept, else dropped (the next pass of
/// that checkout records a fresh tip). A retired tip stays while some ref
/// or worktree HEAD reaches it. A gone worktree's tip is retired while
/// reachable, else dropped.
fn tip_fate(tip: &IndexedTip, liveness: &Liveness, kept: &HashSet<&str>) -> TipFate {
    let reached = liveness.reachable.contains(tip.sha.as_str());
    if tip.worktree.is_retired() {
        return match reached {
            true => TipFate::Keep,
            false => TipFate::Drop,
        };
    }
    let checkout_lives = tip.worktree.is_main() || liveness.live.contains(tip.worktree.as_str());
    match (checkout_lives, reached) {
        (true, true) => TipFate::Keep,
        (true, false) => match liveness.rewinds.get(&tip.worktree) {
            Some(Some(to)) if kept.contains(to.as_str()) => TipFate::Rewind(to.clone()),
            _ => TipFate::Drop,
        },
        (false, true) => TipFate::Retire,
        (false, false) => TipFate::Drop,
    }
}

/// What git says about the repository at prune time.
struct Liveness {
    /// Commits reachable from any ref or worktree HEAD.
    reachable: HashSet<String>,
    /// Names of the linked worktrees git still knows about.
    live: HashSet<String>,
    /// For each unreachable tip of a live checkout: its merge base with
    /// that checkout's current HEAD (`None`: no HEAD or no merge base).
    rewinds: HashMap<WorktreeKey, Option<CommitSha>>,
}

/// Read [`Liveness`] for `snapshot`. Fails closed on any git error.
async fn git_liveness(path: PathBuf, snapshot: Vec<IndexedTip>) -> crate::Result<Liveness> {
    tokio::task::spawn_blocking(move || -> crate::Result<_> {
        fn git_err(what: &str, e: impl std::fmt::Display) -> EngineError {
            EngineError::Git(format!("{what}: {e:#}"))
        }
        let reachable =
            ohara_git::reachable_commit_shas(&path).map_err(|e| git_err("reachable commits", e))?;
        let repo = git2::Repository::discover(&path)
            .map_err(|e| EngineError::Git(format!("open {}: {e}", path.display())))?;
        let names = repo
            .worktrees()
            .map_err(|e| EngineError::Git(format!("list worktrees: {e}")))?;
        let live: HashSet<String> = names.iter().flatten().map(str::to_string).collect();
        let heads = ohara_git::checkout_heads(&path).map_err(|e| git_err("checkout heads", e))?;
        let mut rewinds = HashMap::new();
        for tip in snapshot {
            if tip.worktree.is_retired() || reachable.contains(tip.sha.as_str()) {
                continue;
            }
            let base = match heads.get(tip.worktree.as_str()) {
                Some(head) => ohara_git::merge_base(&path, &tip.sha, head)
                    .map_err(|e| git_err("merge base", e))?,
                None => None,
            };
            rewinds.insert(tip.worktree, base);
        }
        Ok(Liveness {
            reachable,
            live,
            rewinds,
        })
    })
    .await
    .map_err(|e| EngineError::Internal(format!("prune git task: {e}")))?
}
