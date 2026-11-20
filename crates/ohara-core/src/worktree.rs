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

/// Separates the worktree from the sha in a retired tip's key.
const RETIRED_SEPARATOR: char = '/';

/// Identifies a worktree within one repo: empty for the main checkout,
/// a non-empty name for a linked worktree.
///
/// A third, reserved form keys a *retired* tip: the tip a worktree had
/// before it moved to a commit that does not descend from it (branch
/// switch, reset, slot reuse). Its key is `<worktree>/<sha>`. A worktree
/// name is a directory under `.git/worktrees/`, so it can never contain
/// `/`, and a retired key never collides with a real worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeKey(String);

impl WorktreeKey {
    /// Key of the main checkout (empty string).
    pub fn main() -> Self {
        Self(String::new())
    }

    /// Key of a linked worktree; `name` must be non-empty and free of `/`
    /// (git worktree names are directory names).
    pub fn linked(name: &str) -> Result<Self> {
        if name.is_empty() || name.contains(RETIRED_SEPARATOR) {
            return Err(OhraError::InvalidArgument(format!(
                "invalid linked worktree name: {name:?}"
            )));
        }
        Ok(Self(name.to_string()))
    }

    /// Reserved key for `tip`, the tip `of` held before moving to a commit
    /// that does not descend from it.
    pub fn retired(of: &WorktreeKey, tip: &CommitSha) -> Self {
        Self(format!("{}{RETIRED_SEPARATOR}{}", of.0, tip.as_str()))
    }

    /// Parse a key as stored: `""` is main, `<name>/<sha>` is retired,
    /// anything else is a linked worktree name.
    pub fn from_stored(key: &str) -> Result<Self> {
        if key.is_empty() {
            return Ok(Self::main());
        }
        let Some((of, sha)) = key.rsplit_once(RETIRED_SEPARATOR) else {
            return Self::linked(key);
        };
        if of.contains(RETIRED_SEPARATOR) {
            return Err(OhraError::InvalidArgument(format!(
                "invalid retired tip key: {key:?}"
            )));
        }
        let sha = CommitSha::parse(sha)?;
        Ok(Self::retired(&Self(of.to_string()), &sha))
    }

    /// True for the main checkout.
    pub fn is_main(&self) -> bool {
        self.0.is_empty()
    }

    /// True for a retired tip's reserved key (not a worktree).
    pub fn is_retired(&self) -> bool {
        self.0.contains(RETIRED_SEPARATOR)
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

    const P: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn linked_rejects_a_slash_so_it_never_collides_with_a_retired_key() {
        assert!(WorktreeKey::linked("a/b").is_err());
    }

    #[test]
    fn retired_key_is_neither_main_nor_linked_and_round_trips() {
        let sha = CommitSha::parse(P).unwrap();
        for of in [WorktreeKey::main(), WorktreeKey::linked("slot-1").unwrap()] {
            let r = WorktreeKey::retired(&of, &sha);
            assert!(r.is_retired());
            assert!(!r.is_main());
            assert_ne!(r, of);
            assert_eq!(WorktreeKey::from_stored(r.as_str()).unwrap(), r);
        }
        assert!(!WorktreeKey::main().is_retired());
        assert!(!WorktreeKey::linked("slot-1").unwrap().is_retired());
        assert_eq!(WorktreeKey::from_stored("").unwrap(), WorktreeKey::main());
        assert_eq!(
            WorktreeKey::from_stored("slot-1").unwrap(),
            WorktreeKey::linked("slot-1").unwrap()
        );
        assert!(WorktreeKey::from_stored("slot-1/not-a-sha").is_err());
    }

    #[test]
    fn only_main_without_a_main_tip_needs_its_first_run() {
        let slot = WorktreeKey::linked("slot-1").unwrap();
        let tip = |w: WorktreeKey| IndexedTip {
            worktree: w,
            sha: CommitSha::parse(P).unwrap(),
        };
        let linked_only = vec![tip(slot.clone())];
        assert!(main_needs_first_run(&WorktreeKey::main(), &linked_only));
        assert!(main_needs_first_run(&WorktreeKey::main(), &[]));
        assert!(!main_needs_first_run(&slot, &linked_only));
        let with_main = vec![tip(WorktreeKey::main()), tip(slot.clone())];
        assert!(!main_needs_first_run(&WorktreeKey::main(), &with_main));
    }
}
