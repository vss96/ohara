//! Commondir-aware repository identity (red stage).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::GitWalker;
    use git2::Repository;
    use git2::Signature;
    use ohara_core::types::RepoId;
    use std::path::Path;

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
