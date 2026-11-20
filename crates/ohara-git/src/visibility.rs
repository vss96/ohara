//! Commit reachability over libgit2: the [`GitVisibility`] oracle used to
//! scope retrieval to a worktree's HEAD, and [`reachable_commit_shas`] for pruning.

use anyhow::{anyhow, Context, Result};
use ohara_core::{CommitSha, CommitVisibility, OhraError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// [`CommitVisibility`] backed by libgit2 ancestry checks.
pub struct GitVisibility {
    repo_path: PathBuf,
}

impl GitVisibility {
    /// Validate that a repository can be discovered at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        git2::Repository::discover(path).map_err(|e| anyhow!("discover repo: {e:#}"))?;
        Ok(Self {
            repo_path: path.to_path_buf(),
        })
    }
}

fn visible_blocking(path: &Path, head: &str, commits: &[String]) -> Result<Vec<bool>> {
    let repo = git2::Repository::discover(path)?;
    let head_oid = git2::Oid::from_str(head)?;
    repo.find_commit(head_oid)
        .map_err(|e| anyhow!("head {head} not found: {e:#}"))?;
    let mut out = Vec::with_capacity(commits.len());
    for c in commits {
        let Ok(oid) = git2::Oid::from_str(c) else {
            out.push(false);
            continue;
        };
        if repo.find_commit(oid).is_err() {
            out.push(false);
            continue;
        }
        out.push(oid == head_oid || repo.graph_descendant_of(head_oid, oid)?);
    }
    Ok(out)
}

#[async_trait::async_trait]
impl CommitVisibility for GitVisibility {
    async fn visible(
        &self,
        head: &CommitSha,
        commits: &[CommitSha],
    ) -> ohara_core::Result<Vec<bool>> {
        let path = self.repo_path.clone();
        let head = head.as_str().to_string();
        let commits: Vec<String> = commits.iter().map(|c| c.as_str().to_string()).collect();
        tokio::task::spawn_blocking(move || visible_blocking(&path, &head, &commits))
            .await
            .map_err(|e| OhraError::Git(format!("visible: join: {e:#}")))?
            .map_err(|e| OhraError::Git(format!("visible: {e:#}")))
    }
}

/// Every commit reachable from any ref (`refs/*`) or from any worktree's HEAD,
/// including detached ones. Used by prune. Works from the main repo path or
/// from a linked worktree path.
pub fn reachable_commit_shas(path: &Path) -> Result<HashSet<String>> {
    let repo = git2::Repository::discover(path)?;
    // Linked worktree: enumerate from the main repository that owns it.
    let main = match repo.is_worktree() {
        true => {
            // `.git/worktrees/<name>/` -> the shared `.git` dir two levels up.
            let common = repo
                .path()
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| anyhow!("worktree gitdir has no common dir"))?;
            git2::Repository::open(common)?
        }
        false => repo,
    };
    let mut walk = main.revwalk()?;
    walk.push_glob("refs/*")?;
    match walk.push_head() {
        Ok(()) => {}
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {}
        Err(e) => return Err(anyhow::Error::new(e).context("push main HEAD")),
    }
    for name in main.worktrees()?.iter().flatten() {
        let wt_repo = match main
            .find_worktree(name)
            .and_then(|wt| git2::Repository::open_from_worktree(&wt))
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(worktree = %name, error = %e, "skipping unopenable worktree");
                continue;
            }
        };
        let head = match wt_repo.head() {
            Ok(h) => h,
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
                tracing::warn!(worktree = %name, "skipping worktree with unborn HEAD");
                continue;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("read HEAD of worktree {name}")));
            }
        };
        let Some(oid) = head.target() else {
            tracing::debug!(worktree = %name, "worktree HEAD has no direct target; skipping");
            continue;
        };
        walk.push(oid)
            .with_context(|| format!("push HEAD {oid} of worktree {name}"))?;
    }
    let mut out = HashSet::new();
    for oid in walk {
        out.insert(oid?.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use ohara_core::{CommitSha, CommitVisibility};

    struct Fx {
        dir: tempfile::TempDir,
        repo: Repository,
        c: [git2::Oid; 3],
        f1: git2::Oid,
    }

    fn commit(
        repo: &Repository,
        refname: Option<&str>,
        parent: Option<git2::Oid>,
        msg: &str,
    ) -> git2::Oid {
        let sig = Signature::now("a", "a@a").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let parents: Vec<git2::Commit> = parent
            .iter()
            .map(|p| repo.find_commit(*p).unwrap())
            .collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(refname, &sig, &sig, msg, &tree, &refs).unwrap()
    }

    fn fixture() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let c1 = commit(&repo, Some("HEAD"), None, "c1");
        let c2 = commit(&repo, Some("HEAD"), Some(c1), "c2");
        let c3 = commit(&repo, Some("HEAD"), Some(c2), "c3");
        let f1 = commit(&repo, Some("refs/heads/feat"), Some(c2), "f1");
        Fx {
            dir,
            repo,
            c: [c1, c2, c3],
            f1,
        }
    }

    fn sha(o: git2::Oid) -> CommitSha {
        CommitSha::parse(&o.to_string()).unwrap()
    }

    fn all(fx: &Fx) -> Vec<CommitSha> {
        vec![sha(fx.c[0]), sha(fx.c[1]), sha(fx.c[2]), sha(fx.f1)]
    }

    #[tokio::test]
    async fn visible_is_head_or_ancestor() {
        let fx = fixture();
        let v = GitVisibility::open(fx.dir.path()).unwrap();
        let got = v.visible(&sha(fx.c[2]), &all(&fx)).await.unwrap();
        assert_eq!(got, vec![true, true, true, false]);
        let got = v.visible(&sha(fx.f1), &all(&fx)).await.unwrap();
        assert_eq!(got, vec![true, true, false, true]);
    }

    #[tokio::test]
    async fn detached_head_worktree_is_keyed_on_the_commit() {
        let fx = fixture();
        fx.repo.set_head_detached(fx.c[2]).unwrap();
        let v = GitVisibility::open(fx.dir.path()).unwrap();
        let got = v.visible(&sha(fx.f1), &all(&fx)).await.unwrap();
        assert_eq!(got, vec![true, true, false, true]);
    }

    #[tokio::test]
    async fn missing_commit_is_invisible_and_missing_head_is_an_error() {
        let fx = fixture();
        let v = GitVisibility::open(fx.dir.path()).unwrap();
        let missing = CommitSha::parse(&"ab".repeat(20)).unwrap();
        let got = v
            .visible(&sha(fx.c[2]), &[missing.clone(), sha(fx.c[0])])
            .await
            .unwrap();
        assert_eq!(got, vec![false, true]);
        assert!(v.visible(&missing, &[sha(fx.c[0])]).await.is_err());
    }

    #[test]
    fn reachable_includes_branches_and_detached_worktree_heads_but_not_dangling_commits() {
        let fx = fixture();
        let d = commit(&fx.repo, None, Some(fx.c[0]), "dangling");
        let wt_tmp = tempfile::tempdir().unwrap();
        let wt_path = wt_tmp.path().join("wt");
        let wt_path = wt_path.as_path();
        let feat = fx
            .repo
            .find_branch("feat", git2::BranchType::Local)
            .unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(feat.get()));
        fx.repo.worktree("wt", wt_path, Some(&opts)).unwrap();
        let wt_repo = Repository::open(wt_path).unwrap();
        wt_repo.set_head_detached(fx.f1).unwrap();
        drop(feat);
        fx.repo
            .find_branch("feat", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let set = reachable_commit_shas(fx.dir.path()).unwrap();
        for o in [fx.c[0], fx.c[1], fx.c[2], fx.f1] {
            assert!(set.contains(&o.to_string()), "missing {o}");
        }
        assert!(!set.contains(&d.to_string()));
        // Also works from the linked worktree path.
        let set2 = reachable_commit_shas(wt_path).unwrap();
        assert!(set2.contains(&fx.f1.to_string()));
    }

    #[test]
    fn any_unreachable_is_false_only_when_every_commit_is_reached_by_a_ref_or_head() {
        let fx = fixture();
        let dangling = commit(&fx.repo, None, Some(fx.c[1]), "dangling");
        let path = fx.dir.path();
        assert!(!any_unreachable(path, &[]).unwrap());
        // c1 is an ancestor of HEAD; f1 is the `feat` branch tip.
        assert!(!any_unreachable(path, &[sha(fx.c[0]), sha(fx.f1)]).unwrap());
        assert!(any_unreachable(path, &[sha(fx.c[1]), sha(dangling)]).unwrap());
        // A commit the repository does not have is unreachable.
        let missing = CommitSha::parse(&"e".repeat(40)).unwrap();
        assert!(any_unreachable(path, &[missing]).unwrap());
    }

    #[test]
    fn any_unreachable_sees_a_branch_deleted_later() {
        let fx = fixture();
        let path = fx.dir.path();
        assert!(!any_unreachable(path, &[sha(fx.f1)]).unwrap());
        fx.repo
            .find_branch("feat", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        assert!(any_unreachable(path, &[sha(fx.f1)]).unwrap());
    }
}
