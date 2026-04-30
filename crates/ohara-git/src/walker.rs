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

    pub fn first_commit_sha(&self) -> Result<String> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TIME | Sort::REVERSE)?;
        walk.push_head()?;
        let oid = walk.next().context("empty repo")??;
        Ok(oid.to_string())
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
            let parent_sha = c.parent_count().checked_sub(1).map(|_| c.parent(0).ok())
                .flatten()
                .map(|p| p.id().to_string());
            out.push(CommitMeta {
                sha: oid.to_string(),
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
            idx.add_path(std::path::Path::new(&format!("f{i}.txt"))).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parents: Vec<git2::Commit> = parent.iter().map(|p| repo.find_commit(*p).unwrap()).collect();
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            let oid = repo.commit(Some("HEAD"), &sig, &sig, m, &tree, &parent_refs).unwrap();
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
    fn list_commits_since_returns_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["a", "b", "c"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let all = w.list_commits(None).unwrap();
        let mid = &all[1].sha; // "b"
        let after = w.list_commits(Some(mid)).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message.trim(), "c");
    }
}
