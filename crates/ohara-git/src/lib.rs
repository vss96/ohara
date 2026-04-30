//! git2 wrapper: walk commits, extract per-file diffs.

pub mod diff;
pub mod walker;

pub use walker::GitWalker;

use anyhow::Result;
use ohara_core::indexer::CommitSource;
use ohara_core::types::{CommitMeta, Hunk};

pub struct GitCommitSource {
    repo_path: std::path::PathBuf,
}

impl GitCommitSource {
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        // Validate that the path is a discoverable git repo before returning.
        let _ = GitWalker::open(&path)?;
        Ok(Self { repo_path: path.as_ref().to_path_buf() })
    }

    pub fn repo_path(&self) -> &std::path::Path { &self.repo_path }

    /// Open a fresh `GitWalker` for synchronous use. A new walker is returned
    /// each call because `git2::Repository` is `!Sync` and cannot be stored
    /// in a `Send + Sync` `CommitSource` impl.
    pub fn walker(&self) -> Result<GitWalker> { GitWalker::open(&self.repo_path) }
}

#[async_trait::async_trait]
impl CommitSource for GitCommitSource {
    async fn list_commits(&self, since: Option<&str>) -> ohara_core::Result<Vec<CommitMeta>> {
        let since = since.map(str::to_string);
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<CommitMeta>> {
            let w = GitWalker::open(&path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            w.list_commits(since.as_deref())
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }

    async fn hunks_for_commit(&self, sha: &str) -> ohara_core::Result<Vec<Hunk>> {
        let sha = sha.to_string();
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<Hunk>> {
            let repo = git2::Repository::discover(path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            crate::diff::hunks_for_commit(&repo, &sha)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }
}
