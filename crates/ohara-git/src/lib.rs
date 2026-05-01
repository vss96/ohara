//! git2 wrapper: walk commits, extract per-file diffs.

pub mod diff;
pub mod walker;

pub use walker::GitWalker;

use anyhow::Result;
use ohara_core::indexer::CommitSource;
use ohara_core::query::CommitsBehind;
use ohara_core::types::{CommitMeta, Hunk};
use std::sync::{Arc, Mutex};

pub struct GitCommitSource {
    repo_path: std::path::PathBuf,
    repo: Arc<Mutex<git2::Repository>>,
}

impl GitCommitSource {
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let canonical = path.as_ref().to_path_buf();
        let repo = git2::Repository::discover(&canonical)
            .map_err(|e| anyhow::anyhow!("discover repo: {e}"))?;
        // Validate that walker can also open it (sanity check for a clean error path).
        let _ = GitWalker::open(&canonical)?;
        Ok(Self {
            repo_path: canonical,
            repo: Arc::new(Mutex::new(repo)),
        })
    }

    pub fn repo_path(&self) -> &std::path::Path {
        &self.repo_path
    }

    /// Open a fresh `GitWalker` for synchronous use. A new walker is returned
    /// each call because `git2::Repository`'s revwalk borrows mutably and
    /// sharing it across the async boundary is awkward.
    pub fn walker(&self) -> Result<GitWalker> {
        GitWalker::open(&self.repo_path)
    }
}

#[async_trait::async_trait]
impl CommitSource for GitCommitSource {
    #[tracing::instrument(skip(self), fields(repo = %self.repo_path.display()))]
    async fn list_commits(&self, since: Option<&str>) -> ohara_core::Result<Vec<CommitMeta>> {
        let since = since.map(str::to_string);
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<CommitMeta>> {
            // list_commits opens its own GitWalker because Repository's revwalk
            // borrows &self mutably; cleaner to construct a fresh walker per call
            // for now (open() cost is one-time on the GitWalker side).
            let w =
                GitWalker::open(&path).map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            w.list_commits(since.as_deref())
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }

    #[tracing::instrument(skip(self), fields(repo = %self.repo_path.display()))]
    async fn hunks_for_commit(&self, sha: &str) -> ohara_core::Result<Vec<Hunk>> {
        let sha = sha.to_string();
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<Hunk>> {
            let guard = repo
                .lock()
                .map_err(|e| ohara_core::OhraError::Git(format!("repo lock poisoned: {e}")))?;
            crate::diff::hunks_for_commit(&guard, &sha)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }
}

/// Adapter implementing the git-free `CommitsBehind` trait from
/// `ohara-core`. Wraps `GitWalker::list_commits(...).len()`.
pub struct GitCommitsBehind {
    repo_path: std::path::PathBuf,
}

impl GitCommitsBehind {
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let canonical = path.as_ref().to_path_buf();
        // Sanity check that we can open the repo.
        let _ = GitWalker::open(&canonical)?;
        Ok(Self {
            repo_path: canonical,
        })
    }
}

#[async_trait::async_trait]
impl CommitsBehind for GitCommitsBehind {
    async fn count_since(&self, since: Option<&str>) -> ohara_core::Result<u64> {
        let since = since.map(str::to_string);
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<u64> {
            let w =
                GitWalker::open(&path).map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let cs = w
                .list_commits(since.as_deref())
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            Ok(cs.len() as u64)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }
}
