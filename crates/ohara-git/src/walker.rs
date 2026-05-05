use anyhow::{Context, Result};
use git2::{Repository, Sort};
use ohara_core::types::CommitMeta;
use std::path::Path;

pub struct GitWalker {
    repo: Repository,
}

impl GitWalker {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo = Repository::discover(path).context("discover git repo")?;
        Ok(Self { repo })
    }

    /// Walk first-parent chain from HEAD to the topological root commit.
    ///
    /// Used for `RepoId::from_parts` — chosen over author-time-oldest because
    /// rebases and grafted histories can rewrite author dates, while the
    /// first-parent topological root is stable across those operations.
    pub fn first_commit_sha(&self) -> Result<String> {
        let head = self.repo.head().context("HEAD missing")?;
        let mut commit = head.peel_to_commit().context("HEAD is not a commit")?;
        while commit.parent_count() > 0 {
            commit = commit.parent(0).context("first-parent walk failed")?;
        }
        Ok(commit.id().to_string())
    }

    /// SHA at the tip of the current branch. O(1) — does not walk history.
    pub fn head_commit_sha(&self) -> Result<String> {
        let head = self.repo.head().context("HEAD missing")?;
        let commit = head.peel_to_commit().context("HEAD is not a commit")?;
        Ok(commit.id().to_string())
    }

    /// Stream `(CommitMeta, changed-paths)` pairs to a callback in
    /// topological-reverse (oldest-first) order. Used by `ohara plan`
    /// for the diff-only pre-flight walk on giant repos. Memory-bounded:
    /// no full Vec materialised.
    ///
    /// "Changed paths" = paths in the diff of the commit vs its first
    /// parent. Initial commits diff against the empty tree.
    pub fn for_each_commit_paths<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(&CommitMeta, &[String]) -> Result<()>,
    {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.push_head()?;

        let mut paths_buf: Vec<String> = Vec::with_capacity(16);
        for oid in walk {
            let oid = oid?;
            let c = self.repo.find_commit(oid)?;
            let parent_sha = if c.parent_count() > 0 {
                c.parent(0).ok().map(|p| p.id().to_string())
            } else {
                None
            };
            let meta = CommitMeta {
                commit_sha: oid.to_string(),
                parent_sha: parent_sha.clone(),
                is_merge: c.parent_count() > 1,
                author: Some(c.author().name().unwrap_or("").to_string()).filter(|s| !s.is_empty()),
                ts: c.time().seconds(),
                message: c.message().unwrap_or("").to_string(),
            };

            paths_buf.clear();
            collect_changed_paths(&self.repo, &c, &mut paths_buf)?;
            callback(&meta, &paths_buf)?;
        }
        Ok(())
    }

    pub fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.push_head()?;
        if let Some(s) = since {
            // Hide the watermark and its ancestors so we get only newer commits.
            let oid = git2::Oid::from_str(s)?;
            walk.hide(oid)?;
        }
        let mut out = Vec::new();
        for oid in walk {
            let oid = oid?;
            let c = self.repo.find_commit(oid)?;
            let parent_sha = if c.parent_count() > 0 {
                c.parent(0).ok().map(|p| p.id().to_string())
            } else {
                None
            };
            out.push(CommitMeta {
                commit_sha: oid.to_string(),
                parent_sha,
                is_merge: c.parent_count() > 1,
                author: Some(c.author().name().unwrap_or("").to_string()).filter(|s| !s.is_empty()),
                ts: c.time().seconds(),
                message: c.message().unwrap_or("").to_string(),
            });
        }
        Ok(out)
    }
}

fn collect_changed_paths(
    repo: &Repository,
    commit: &git2::Commit<'_>,
    out: &mut Vec<String>,
) -> Result<()> {
    let new_tree = commit.tree().context("commit tree")?;
    let old_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)
                .context("first parent")?
                .tree()
                .context("parent tree")?,
        )
    } else {
        None
    };

    // Path-only diff: skip binary check, no untracked.
    let mut opts = git2::DiffOptions::new();
    opts.skip_binary_check(true).include_untracked(false);
    let diff = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
        .context("diff_tree_to_tree paths-only")?;

    diff.foreach(
        &mut |delta, _progress| {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                out.push(p.to_string_lossy().into_owned());
            }
            true
        },
        None,
        None,
        None,
    )
    .context("diff foreach")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;

    fn init_repo_with_commits(dir: &std::path::Path, msgs: &[&str]) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        let mut parent: Option<git2::Oid> = None;
        for (i, m) in msgs.iter().enumerate() {
            fs::write(dir.join(format!("f{i}.txt")), format!("v{i}")).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new(&format!("f{i}.txt")))
                .unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parents: Vec<git2::Commit> = parent
                .iter()
                .map(|p| repo.find_commit(*p).unwrap())
                .collect();
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            let oid = repo
                .commit(Some("HEAD"), &sig, &sig, m, &tree, &parent_refs)
                .unwrap();
            parent = Some(oid);
        }
        repo
    }

    #[test]
    fn list_commits_in_topological_reverse_order() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["a", "b", "c"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let cs = w.list_commits(None).unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].message.trim(), "a");
        assert_eq!(cs[2].message.trim(), "c");
    }

    #[test]
    fn for_each_commit_paths_visits_all_commits_in_topo_order() {
        // Plan 26 Task B.1: stream commit-path pairs to a callback in
        // topological-reverse order (oldest first). Each commit's path
        // list reflects the files changed in that commit.
        let dir = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_commits(dir.path(), &["c1", "c2", "c3"]);
        let walker = GitWalker::open(dir.path()).unwrap();

        let mut seen_msgs: Vec<String> = Vec::new();
        let mut seen_paths: Vec<Vec<String>> = Vec::new();
        walker
            .for_each_commit_paths(|meta, paths| {
                seen_msgs.push(meta.message.trim().to_string());
                seen_paths.push(paths.to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(seen_msgs, vec!["c1", "c2", "c3"]);
        // init_repo_with_commits writes f0.txt, f1.txt, f2.txt — one
        // per commit. Each commit changes exactly one file (no overlap).
        assert_eq!(seen_paths.len(), 3);
        assert!(seen_paths[0].iter().any(|p| p == "f0.txt"));
        assert!(seen_paths[1].iter().any(|p| p == "f1.txt"));
        assert!(seen_paths[2].iter().any(|p| p == "f2.txt"));
    }

    #[test]
    fn for_each_commit_paths_callback_error_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = init_repo_with_commits(dir.path(), &["c1", "c2", "c3"]);
        let walker = GitWalker::open(dir.path()).unwrap();
        let mut visited = 0;
        let res = walker.for_each_commit_paths(|_, _| {
            visited += 1;
            if visited == 2 {
                Err(anyhow::anyhow!("stop"))
            } else {
                Ok(())
            }
        });
        assert!(res.is_err(), "expected callback error to propagate");
        assert_eq!(visited, 2, "must stop after callback error");
    }

    #[test]
    fn list_commits_since_returns_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["a", "b", "c"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let all = w.list_commits(None).unwrap();
        let mid = &all[1].commit_sha; // "b"
        let after = w.list_commits(Some(mid)).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message.trim(), "c");
    }

    #[test]
    fn first_commit_sha_walks_to_topological_root() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["root", "second", "third"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let first = w.first_commit_sha().unwrap();
        let cs = w.list_commits(None).unwrap();
        // The root commit (cs[0] in topological-reverse order) is the one without a parent.
        assert_eq!(first, cs[0].commit_sha);
        assert!(
            cs[0].parent_sha.is_none(),
            "root commit should have no parent"
        );
        assert!(
            cs[1].parent_sha.is_some(),
            "non-root commit should have a parent"
        );
    }
}
