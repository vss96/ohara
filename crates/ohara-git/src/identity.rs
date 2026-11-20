//! Commondir-aware repository identity.
//!
//! A git clone and all of its linked worktrees must resolve to the same
//! [`RepoId`] so they share one index. Plain clones keep the legacy
//! formula (hash of the canonicalized argument path) byte-for-byte.

use crate::walker::GitWalker;
use anyhow::{Context, Result};
use git2::Repository;
use ohara_core::types::RepoId;
use ohara_core::WorktreeKey;
use std::path::{Path, PathBuf};

/// Resolved identity of the repository (and worktree) at a path.
#[derive(Debug, Clone)]
pub struct RepoIdentity {
    /// Id shared by a clone and all of its linked worktrees.
    pub repo_id: RepoId,
    /// Canonical path of the worktree the caller is in (what git2 should open).
    pub worktree_path: PathBuf,
    /// The string that was hashed into `repo_id`.
    pub id_path: String,
    /// First-parent root commit of the (main) repository's HEAD.
    pub first_commit_sha: String,
    /// Which worktree of the clone this is.
    pub worktree: WorktreeKey,
}

impl RepoIdentity {
    /// Resolve the identity of the repository containing `path`.
    ///
    /// Non-worktree paths hash the canonicalized argument exactly as the
    /// legacy code did. Linked worktrees hash the main repository's
    /// workdir (or the bare repo path) so they share the main clone's id.
    pub fn resolve(path: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("canonicalize {}", path.display()))?;
        let repo = Repository::discover(&canonical).context("discover git repo")?;
        if !repo.is_worktree() {
            let first = GitWalker::open(&canonical)?.first_commit_sha()?;
            return Ok(Self::build(
                first,
                canonical.to_string_lossy().into_owned(),
                canonical,
                WorktreeKey::main(),
            ));
        }
        Self::resolve_linked(&repo)
    }

    fn resolve_linked(repo: &Repository) -> Result<Self> {
        let commondir = commondir_of(repo)?;
        let main = Repository::open(&commondir).context("open main repository")?;
        let id_base = match main.workdir() {
            Some(dir) => dir.to_path_buf(),
            None => commondir.clone(),
        };
        let id_path = std::fs::canonicalize(&id_base)
            .with_context(|| format!("canonicalize {}", id_base.display()))?
            .to_string_lossy()
            .into_owned();
        let first = match GitWalker::from_repository(main).first_commit_sha() {
            Ok(sha) => sha,
            Err(_) => {
                GitWalker::from_repository(Repository::open(repo.path())?).first_commit_sha()?
            }
        };
        let name = repo
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .context("worktree admin dir has no name")?;
        let worktree = WorktreeKey::linked(name).map_err(|e| anyhow::anyhow!("{e}"))?;
        let workdir = repo.workdir().context("linked worktree has no workdir")?;
        let worktree_path = std::fs::canonicalize(workdir)
            .with_context(|| format!("canonicalize {}", workdir.display()))?;
        Ok(Self::build(first, id_path, worktree_path, worktree))
    }

    fn build(
        first_commit_sha: String,
        id_path: String,
        worktree_path: PathBuf,
        worktree: WorktreeKey,
    ) -> Self {
        Self {
            repo_id: RepoId::from_parts(&first_commit_sha, &id_path),
            worktree_path,
            id_path,
            first_commit_sha,
            worktree,
        }
    }
}

/// Common git dir of a linked worktree. git2 0.18 has no `commondir()`,
/// so read the `commondir` file in the worktree's admin dir (a path,
/// usually relative, to the main `.git`).
fn commondir_of(repo: &Repository) -> Result<PathBuf> {
    let admin = repo.path();
    let raw = std::fs::read_to_string(admin.join("commondir"))
        .with_context(|| format!("read commondir in {}", admin.display()))?;
    std::fs::canonicalize(admin.join(raw.trim())).context("canonicalize commondir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    fn commit_empty(repo: &Repository, update_ref: Option<&str>, msg: &str) -> git2::Oid {
        let sig = Signature::now("a", "a@a").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(update_ref, &sig, &sig, msg, &tree, &[])
            .unwrap()
    }

    fn init_repo_with_commit(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        commit_empty(&repo, Some("HEAD"), "root");
        repo
    }

    fn init_bare_repo_with_commit(dir: &Path) -> Repository {
        let repo = Repository::init_bare(dir).unwrap();
        commit_empty(&repo, Some("HEAD"), "root");
        repo
    }

    fn add_worktree(repo: &Repository, name: &str, path: &Path) {
        repo.worktree(name, path, None).unwrap();
    }

    fn commit_orphan_root_on_head(wt: &Path) {
        let repo = Repository::open(wt).unwrap();
        let oid = commit_empty(&repo, None, "orphan root");
        let head = repo.find_reference("HEAD").unwrap();
        let target = head.symbolic_target().unwrap().to_string();
        repo.reference(&target, oid, true, "orphan").unwrap();
    }

    #[test]
    fn plain_clone_id_is_byte_identical_to_legacy_formula() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commit(dir.path());
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let first = GitWalker::open(&canonical)
            .unwrap()
            .first_commit_sha()
            .unwrap();
        let legacy = RepoId::from_parts(&first, &canonical.to_string_lossy());
        let id = RepoIdentity::resolve(dir.path()).unwrap();
        assert_eq!(id.repo_id, legacy);
        assert!(id.worktree.is_main());
    }

    #[test]
    fn subdirectory_argument_keeps_its_legacy_id() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commit(dir.path());
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let canonical = std::fs::canonicalize(&sub).unwrap();
        let first = GitWalker::open(&canonical)
            .unwrap()
            .first_commit_sha()
            .unwrap();
        assert_eq!(
            RepoIdentity::resolve(&sub).unwrap().repo_id,
            RepoId::from_parts(&first, &canonical.to_string_lossy())
        );
    }

    #[test]
    fn linked_worktree_shares_main_id_and_reports_its_name() {
        let main = tempfile::tempdir().unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let repo = init_repo_with_commit(main.path());
        add_worktree(&repo, "slot-1", &wt_parent.path().join("slot-1"));
        let main_id = RepoIdentity::resolve(main.path()).unwrap();
        let wt_id = RepoIdentity::resolve(&wt_parent.path().join("slot-1")).unwrap();
        assert_eq!(wt_id.repo_id, main_id.repo_id);
        assert_eq!(wt_id.worktree.as_str(), "slot-1");
        assert_ne!(wt_id.worktree_path, main_id.worktree_path);
    }

    #[test]
    fn separate_clones_get_distinct_ids() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        init_repo_with_commit(a.path());
        git2::Repository::clone(a.path().to_str().unwrap(), b.path()).unwrap();
        assert_ne!(
            RepoIdentity::resolve(a.path()).unwrap().repo_id,
            RepoIdentity::resolve(b.path()).unwrap().repo_id
        );
    }

    #[test]
    fn worktree_of_bare_repo_resolves_to_the_bare_path() {
        let bare = tempfile::tempdir().unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let repo = init_bare_repo_with_commit(bare.path());
        add_worktree(&repo, "w", &wt_parent.path().join("w"));
        let id = RepoIdentity::resolve(&wt_parent.path().join("w")).unwrap();
        assert_eq!(
            id.id_path,
            std::fs::canonicalize(bare.path())
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn linked_worktree_on_orphan_branch_still_shares_main_id() {
        let main = tempfile::tempdir().unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let repo = init_repo_with_commit(main.path());
        let wt = wt_parent.path().join("orphan");
        add_worktree(&repo, "orphan", &wt);
        commit_orphan_root_on_head(&wt);
        assert_eq!(
            RepoIdentity::resolve(&wt).unwrap().repo_id,
            RepoIdentity::resolve(main.path()).unwrap().repo_id
        );
    }
}
