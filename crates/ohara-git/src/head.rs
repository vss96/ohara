//! The one place that reads a checkout's HEAD commit (plan-33).

use anyhow::{anyhow, Context, Result};
use ohara_core::CommitSha;
use std::path::Path;

/// HEAD commit of the checkout containing `path`. The repository is
/// discovered upward from `path`, so a subdirectory of a clone or of a
/// linked worktree works. `Ok(None)` means HEAD is an unborn branch (no
/// commits yet); any other failure to read HEAD is an error.
pub fn read_head(path: &Path) -> Result<Option<CommitSha>> {
    let repo = git2::Repository::discover(path)
        .with_context(|| format!("discover git repo at {}", path.display()))?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context("read HEAD")),
    };
    let commit = head.peel_to_commit().context("HEAD is not a commit")?;
    let sha = CommitSha::parse(&commit.id().to_string()).map_err(|e| anyhow!("{e}"))?;
    Ok(Some(sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    fn commit(repo: &Repository) -> git2::Oid {
        let sig = Signature::now("a", "a@a").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "root", &tree, &[])
            .unwrap()
    }

    #[test]
    fn reads_head_from_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let oid = commit(&repo);
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        let head = read_head(&sub).unwrap().unwrap();
        assert_eq!(head.as_str(), oid.to_string());
    }

    #[test]
    fn unborn_head_is_none() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        assert!(read_head(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_path_outside_any_repo_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_head(dir.path()).is_err());
    }
}
