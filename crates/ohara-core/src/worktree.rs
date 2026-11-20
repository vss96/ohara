//! Newtypes and the persistence contract for worktree-aware indexing.

use crate::error::{OhraError, Result};
use crate::types::RepoId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A full 40-character lowercase hex git commit id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitSha(String);

impl CommitSha {
    /// Validate and wrap a 40-char lowercase hex string.
    pub fn parse(hex: &str) -> Result<Self> {
        let valid = hex.len() == 40 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !valid {
            return Err(OhraError::InvalidArgument(format!(
                "invalid commit sha: {hex:?}"
            )));
        }
        Ok(Self(hex.to_string()))
    }

    /// The hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identifies a worktree within one repo: empty for the main checkout,
/// a non-empty name for a linked worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeKey(String);

impl WorktreeKey {
    /// Key of the main checkout (empty string).
    pub fn main() -> Self {
        Self(String::new())
    }

    /// Key of a linked worktree; `name` must be non-empty.
    pub fn linked(name: &str) -> Result<Self> {
        if name.is_empty() {
            return Err(OhraError::InvalidArgument(
                "linked worktree name must not be empty".into(),
            ));
        }
        Ok(Self(name.to_string()))
    }

    /// True for the main checkout.
    pub fn is_main(&self) -> bool {
        self.0.is_empty()
    }

    /// The key string ("" for main).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The last indexed commit for one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedTip {
    /// Which worktree the tip belongs to.
    pub worktree: WorktreeKey,
    /// The commit indexed as that worktree's tip.
    pub sha: CommitSha,
}

/// Persistence of per-worktree indexed tips and commit pruning.
#[async_trait]
pub trait TipStore: Send + Sync {
    /// All recorded tips for the repo.
    async fn indexed_tips(&self, repo_id: &RepoId) -> Result<Vec<IndexedTip>>;

    /// Insert or replace the tip for `worktree`.
    async fn set_indexed_tip(
        &self,
        repo_id: &RepoId,
        worktree: &WorktreeKey,
        sha: &CommitSha,
    ) -> Result<()>;

    /// Remove the tip for `worktree`, if any.
    async fn remove_indexed_tip(&self, repo_id: &RepoId, worktree: &WorktreeKey) -> Result<()>;

    /// Shas of every commit currently indexed for the repo.
    async fn indexed_commit_shas(&self, repo_id: &RepoId) -> Result<Vec<CommitSha>>;

    /// Deletes each commit with its hunks, vectors and FTS rows. Returns commits deleted.
    async fn delete_commits(&self, repo_id: &RepoId, shas: &[CommitSha]) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_sha_accepts_40_hex_and_rejects_everything_else() {
        assert!(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(CommitSha::parse("abc").is_err());
        assert!(CommitSha::parse("0123456789ABCDEF0123456789abcdef01234567").is_err());
        assert!(CommitSha::parse("z123456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn worktree_key_main_is_empty_and_linked_rejects_empty() {
        assert_eq!(WorktreeKey::main().as_str(), "");
        assert!(WorktreeKey::main().is_main());
        assert!(WorktreeKey::linked("").is_err());
        let k = WorktreeKey::linked("slot-1").unwrap();
        assert_eq!(k.as_str(), "slot-1");
        assert!(!k.is_main());
    }
}
