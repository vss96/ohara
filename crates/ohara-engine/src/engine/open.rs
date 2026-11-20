//! Opening and evicting per-repo stores and per-worktree handles
//! (plan-33): one [`RepoStore`] per [`RepoId`], one [`RepoHandle`] per
//! canonical worktree path.

use super::RetrievalEngine;
use crate::error::EngineError;
use crate::handle::{RepoHandle, RepoStore};
use ohara_core::types::RepoId;
use ohara_git::RepoIdentity;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Resolve the repo/worktree identity of `repo_path`, mapping the git
/// layer's error into [`EngineError::Git`].
fn resolve_identity(repo_path: &Path) -> crate::Result<RepoIdentity> {
    RepoIdentity::resolve(repo_path)
        .map_err(|e| EngineError::Git(format!("resolve repo identity: {e}")))
}

/// Opens racing `invalidate_repo` retry this often before returning an
/// uncached handle.
const OPEN_ATTEMPTS: usize = 3;

/// Whether a cached handle still belongs to `identity`: a path can be
/// reused by another clone, or by a worktree with another admin name.
fn matches(handle: &RepoHandle, identity: &RepoIdentity) -> bool {
    handle.repo_id == identity.repo_id && handle.worktree == identity.worktree
}

impl RetrievalEngine {
    /// Open (or return cached) handle for the worktree containing
    /// `repo_path`.
    ///
    /// Resolves the [`RepoIdentity`] (linked worktrees share the main
    /// clone's id), reuses or opens the repo-wide [`RepoStore`], then
    /// builds the per-worktree handle (its own path, worktree key and
    /// blamer). Handles are cached by canonical worktree path and reused
    /// only while they match the resolved repo id and worktree key.
    pub async fn open_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<Arc<RepoHandle>> {
        let identity = resolve_identity(repo_path.as_ref())?;
        let mut handle = None;
        for _ in 0..OPEN_ATTEMPTS {
            if let Some(existing) = self.cached_handle(&identity).await {
                return Ok(existing);
            }
            let generation = self.store_generation.load(Ordering::SeqCst);
            let fresh = self.build_handle(&identity).await?;
            // Re-check under the write lock: another open may have won the
            // race, and an invalidate since `generation` evicted our store.
            let mut w = self.repos.write().await;
            if let Some(existing) = w.get(&identity.worktree_path) {
                if matches(existing, &identity) {
                    return Ok(existing.clone());
                }
            }
            if self.store_generation.load(Ordering::SeqCst) == generation {
                w.insert(identity.worktree_path.clone(), fresh.clone());
                return Ok(fresh);
            }
            handle = Some(fresh);
        }
        tracing::debug!(path = %identity.worktree_path.display(), "open_repo kept racing invalidate_repo; not caching");
        handle.ok_or_else(|| EngineError::Internal("invariant: OPEN_ATTEMPTS > 0".into()))
    }

    /// The cached handle for `identity`'s worktree path, if it matches.
    async fn cached_handle(&self, identity: &RepoIdentity) -> Option<Arc<RepoHandle>> {
        let r = self.repos.read().await;
        let existing = r.get(&identity.worktree_path)?;
        match matches(existing, identity) {
            true => Some(existing.clone()),
            false => None,
        }
    }

    /// A new handle for `identity` over the (cached or newly opened) store.
    async fn build_handle(&self, identity: &RepoIdentity) -> crate::Result<Arc<RepoHandle>> {
        tracing::debug!(
            repo_id = ?identity.repo_id,
            path = %identity.worktree_path.display(),
            worktree = identity.worktree.as_str(),
            "open_repo cache miss"
        );
        let store = self.open_store(identity).await?;
        let blamer = Arc::new(
            ohara_git::Blamer::open(&identity.worktree_path)
                .map_err(|e| EngineError::Git(format!("blamer open: {e}")))?,
        );
        Ok(Arc::new(RepoHandle {
            repo_id: identity.repo_id.clone(),
            repo_path: identity.worktree_path.clone(),
            worktree: identity.worktree.clone(),
            store,
            blamer,
        }))
    }

    /// Return the cached [`RepoStore`] for `identity.repo_id`, opening
    /// SQLite and building the retriever on a miss. The one
    /// `SqliteStorage` serves as both `storage` and `tips`. A store opened
    /// while `invalidate_repo` ran is returned but not cached.
    async fn open_store(&self, identity: &RepoIdentity) -> crate::Result<Arc<RepoStore>> {
        if let Some(existing) = self.stores.read().await.get(&identity.repo_id) {
            return Ok(existing.clone());
        }
        let generation = self.store_generation.load(Ordering::SeqCst);

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
        if self.store_generation.load(Ordering::SeqCst) == generation {
            w.insert(identity.repo_id.clone(), store.clone());
        }
        Ok(store)
    }

    /// Evict the cached [`RepoStore`], **every** [`RepoHandle`] of the
    /// same repository (all worktrees), and their meta / blame cache
    /// entries.
    ///
    /// After this call, the next [`open_repo`][Self::open_repo] for any
    /// worktree of the repo re-opens storage and rebuilds its handle. The
    /// generation bump makes an open already in flight retry instead of
    /// caching the evicted store.
    pub async fn invalidate_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<()> {
        let repo_id = resolve_identity(repo_path.as_ref())?.repo_id;
        {
            let mut stores = self.stores.write().await;
            self.store_generation.fetch_add(1, Ordering::SeqCst);
            stores.remove(&repo_id);
        }
        self.repos
            .write()
            .await
            .retain(|_, handle| handle.repo_id != repo_id);
        self.repo_caches.invalidate_repo(&repo_id);
        Ok(())
    }

    /// Drop the caches an index or prune job makes stale for `repo_id`:
    /// the whole [`crate::cache::RepoCaches`] group (meta entries of every
    /// worktree plus blame entries). The query `EmbeddingCache` and the `AncestryCache` are
    /// not repo state, so they stay.
    pub(crate) fn refresh_repo_caches(&self, repo_id: &RepoId) {
        // Keep RepoStore/RepoHandles: same DB file, and dropping them races open_repo into a second pool.
        self.repo_caches.invalidate_repo(repo_id);
    }
}
