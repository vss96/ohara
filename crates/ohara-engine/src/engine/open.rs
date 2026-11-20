//! Opening and evicting per-repo stores and per-worktree handles
//! (plan-33): one [`RepoStore`] per [`RepoId`], one [`RepoHandle`] per
//! canonical worktree path.

use super::RetrievalEngine;
use crate::error::EngineError;
use crate::handle::{RepoHandle, RepoStore};
use ohara_git::RepoIdentity;
use std::path::Path;
use std::sync::Arc;

/// Resolve the repo/worktree identity of `repo_path`, mapping the git
/// layer's error into [`EngineError::Git`].
fn resolve_identity(repo_path: &Path) -> crate::Result<RepoIdentity> {
    RepoIdentity::resolve(repo_path)
        .map_err(|e| EngineError::Git(format!("resolve repo identity: {e}")))
}

impl RetrievalEngine {
    /// Open (or return cached) handle for the worktree containing
    /// `repo_path`.
    ///
    /// Resolves the [`RepoIdentity`] (linked worktrees share the main
    /// clone's id), reuses or opens the repo-wide [`RepoStore`], then
    /// builds the per-worktree handle (its own path, worktree key and
    /// blamer). Handles are cached by canonical worktree path.
    pub async fn open_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<Arc<RepoHandle>> {
        let identity = resolve_identity(repo_path.as_ref())?;

        if let Some(existing) = self.repos.read().await.get(&identity.worktree_path) {
            return Ok(existing.clone());
        }

        tracing::debug!(
            repo_id = ?identity.repo_id,
            path = %identity.worktree_path.display(),
            worktree = identity.worktree.as_str(),
            "open_repo cache miss"
        );
        let store = self.open_store(&identity).await?;
        let blamer = Arc::new(
            ohara_git::Blamer::open(&identity.worktree_path)
                .map_err(|e| EngineError::Git(format!("blamer open: {e}")))?,
        );
        let handle = Arc::new(RepoHandle {
            repo_id: identity.repo_id,
            repo_path: identity.worktree_path.clone(),
            worktree: identity.worktree,
            store,
            blamer,
        });

        // Re-check under write lock to avoid double-open race.
        let mut w = self.repos.write().await;
        if let Some(existing) = w.get(&identity.worktree_path) {
            return Ok(existing.clone());
        }
        w.insert(identity.worktree_path, handle.clone());
        Ok(handle)
    }

    /// Return the cached [`RepoStore`] for `identity.repo_id`, opening
    /// SQLite and building the retriever on a miss. The one
    /// `SqliteStorage` serves as both `storage` and `tips`.
    async fn open_store(&self, identity: &RepoIdentity) -> crate::Result<Arc<RepoStore>> {
        if let Some(existing) = self.stores.read().await.get(&identity.repo_id) {
            return Ok(existing.clone());
        }

        let db_path = ohara_core::paths::index_db_path(&identity.repo_id)
            .map_err(|e| EngineError::Internal(format!("index_db_path: {e}")))?;
        let sqlite = Arc::new(
            ohara_storage::SqliteStorage::open(&db_path)
                .await
                .map_err(|e| EngineError::Storage(format!("open: {e}")))?,
        );
        let storage: Arc<dyn ohara_core::Storage> = sqlite.clone();
        let tips: Arc<dyn ohara_core::TipStore> = sqlite;
        let retriever = ohara_core::Retriever::new(storage.clone(), self.embedder.clone())
            .with_reranker(self.reranker.clone());
        let store = Arc::new(RepoStore {
            repo_id: identity.repo_id.clone(),
            storage,
            tips,
            retriever,
        });

        let mut w = self.stores.write().await;
        if let Some(existing) = w.get(&identity.repo_id) {
            return Ok(existing.clone());
        }
        w.insert(identity.repo_id.clone(), store.clone());
        Ok(store)
    }

    /// Evict the cached [`RepoStore`], **every** [`RepoHandle`] of the
    /// same repository (all worktrees), and their meta / blame cache
    /// entries.
    ///
    /// After this call, the next [`open_repo`][Self::open_repo] for any
    /// worktree of the repo re-opens storage and rebuilds its handle.
    pub async fn invalidate_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<()> {
        let repo_id = resolve_identity(repo_path.as_ref())?.repo_id;
        self.stores.write().await.remove(&repo_id);
        self.repos
            .write()
            .await
            .retain(|_, handle| handle.repo_id != repo_id);
        self.meta_cache.invalidate(&repo_id);
        self.blame_cache.invalidate_repo(&repo_id);
        Ok(())
    }
}
