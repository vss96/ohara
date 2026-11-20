//! Worktree-scoped retrieval (plan-33): hide commits that the querying
//! worktree's HEAD cannot reach.

use super::RetrievalEngine;
use crate::cache::{AncestryCache, CachedVisibility};
use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::query::{PatternHit, PatternQuery};
use ohara_core::{CommitSha, CommitVisibility, VisibilityScope};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// HEAD commit of the checkout containing `repo_path` (discovered upward,
/// so a subdirectory works), read off the async executor. `None` when HEAD
/// is an unborn branch.
pub(crate) async fn read_head(repo_path: &Path) -> crate::Result<Option<CommitSha>> {
    let path = repo_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        ohara_git::read_head(&path).map_err(|e| EngineError::Git(format!("read HEAD: {e:#}")))
    })
    .await
    .map_err(|e| EngineError::Internal(format!("head task: {e}")))?
}

impl RetrievalEngine {
    /// The engine-wide ancestry cache shared by every visibility oracle.
    pub(crate) fn ancestry_cache(&self) -> Arc<AncestryCache> {
        self.ancestry.clone()
    }

    /// Number of `find_pattern` calls that needed the ancestry filter
    /// (some indexed tip was not reachable from the querying HEAD).
    pub fn ancestry_filtered_queries(&self) -> u64 {
        self.ancestry_filtered_count.load(Ordering::Relaxed)
    }

    /// Perf-harness only (`perf-hooks` feature): skip the visibility step
    /// (tip check and post-filter; HEAD is still read) so a benchmark can
    /// measure what the filter adds. Never enabled by the shipped binaries.
    #[cfg(feature = "perf-hooks")]
    #[doc(hidden)]
    pub fn set_visibility_bypass(&self, bypass: bool) {
        self.visibility_bypass.store(bypass, Ordering::Relaxed);
    }

    /// Run the retriever for `handle`, filtering to commits visible from
    /// `head`, the worktree's HEAD. When every indexed tip is visible (or
    /// none exist) the unfiltered fast path is taken.
    pub(super) async fn retrieve_visible(
        &self,
        handle: &RepoHandle,
        head: &CommitSha,
        query: &PatternQuery,
        now_unix: i64,
    ) -> crate::Result<Vec<PatternHit>> {
        if self.visibility_bypass.load(Ordering::Relaxed) {
            let (hits, _profile) = handle
                .store
                .retriever
                .find_pattern_scoped(&handle.repo_id, query, now_unix, None)
                .await
                .map_err(EngineError::from)?;
            return Ok(hits);
        }
        let tips = handle
            .store
            .tips
            .indexed_tips(&handle.repo_id)
            .await
            .map_err(EngineError::from)?;
        let git = ohara_git::GitVisibility::open(&handle.repo_path)
            .map_err(|e| EngineError::Git(format!("open visibility oracle: {e:#}")))?;
        let oracle = CachedVisibility::new(Arc::new(git), self.ancestry.clone());
        let tip_shas: Vec<CommitSha> = tips.into_iter().map(|t| t.sha).collect();
        let verdicts = oracle
            .visible(head, &tip_shas)
            .await
            .map_err(EngineError::from)?;
        let all_visible = verdicts.iter().all(|v| *v);
        let scope = (!all_visible).then_some(VisibilityScope {
            head,
            oracle: &oracle,
        });
        if scope.is_some() {
            self.ancestry_filtered_count.fetch_add(1, Ordering::Relaxed);
        }
        let (hits, _profile) = handle
            .store
            .retriever
            .find_pattern_scoped(&handle.repo_id, query, now_unix, scope)
            .await
            .map_err(EngineError::from)?;
        Ok(hits)
    }
}
