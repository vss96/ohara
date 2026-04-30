use anyhow::{Context, Result};
use git2::{DiffFormat, DiffOptions, Oid, Repository};
use ohara_core::types::{ChangeKind, Hunk};
use std::path::Path;

pub fn hunks_for_commit(repo: &Repository, sha: &str) -> Result<Vec<Hunk>> {
    let oid = Oid::from_str(sha).context("parse oid")?;
    let commit = repo.find_commit(oid).context("find commit")?;
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let mut opts = DiffOptions::new();
    opts.context_lines(3).interhunk_lines(0).ignore_whitespace_eol(true);

    let diff = match parent_tree.as_ref() {
        Some(p) => repo.diff_tree_to_tree(Some(p), Some(&tree), Some(&mut opts))?,
        None => repo.diff_tree_to_tree(None, Some(&tree), Some(&mut opts))?,
    };

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<(String, ChangeKind)> = None;
    let mut buf = String::new();

    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta.new_file().path().or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let ck = match delta.status() {
            git2::Delta::Added => ChangeKind::Added,
            git2::Delta::Deleted => ChangeKind::Deleted,
            git2::Delta::Renamed => ChangeKind::Renamed,
            _ => ChangeKind::Modified,
        };
        match &current {
            Some((p, _)) if *p != path => {
                hunks.push(make_hunk(sha, p, current.as_ref().unwrap().1, std::mem::take(&mut buf)));
                current = Some((path.clone(), ck));
            }
            None => current = Some((path.clone(), ck)),
            _ => {}
        }
        let prefix = match line.origin() {
            '+' | '-' | ' ' => format!("{}", line.origin()),
            _ => String::new(),
        };
        buf.push_str(&prefix);
        buf.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;

    if let Some((p, ck)) = current.take() {
        hunks.push(make_hunk(sha, &p, ck, buf));
    }
    Ok(hunks)
}

fn make_hunk(sha: &str, file_path: &str, ck: ChangeKind, diff_text: String) -> Hunk {
    let language = detect_language(file_path);
    Hunk {
        commit_sha: sha.to_string(),
        file_path: file_path.to_string(),
        language,
        change_kind: ck,
        diff_text,
    }
}

fn detect_language(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        _ => return None,
    }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    fn init_with_two_commits(dir: &std::path::Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("a.rs")).unwrap(); idx.write().unwrap();
        let t1 = idx.write_tree().unwrap();
        let c1 = repo.commit(Some("HEAD"), &sig, &sig, "first", &repo.find_tree(t1).unwrap(), &[]).unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() { println!(); }\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("a.rs")).unwrap(); idx.write().unwrap();
        let t2 = idx.write_tree().unwrap();
        {
            let p = repo.find_commit(c1).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "second", &repo.find_tree(t2).unwrap(), &[&p]).unwrap();
        }
        repo
    }

    #[test]
    fn extract_diff_for_modifying_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_with_two_commits(dir.path());
        let mut walk = repo.revwalk().unwrap();
        walk.set_sorting(git2::Sort::TIME).unwrap();
        walk.push_head().unwrap();
        let head = walk.next().unwrap().unwrap().to_string();

        let hunks = hunks_for_commit(&repo, &head).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file_path, "a.rs");
        assert!(matches!(hunks[0].change_kind, ChangeKind::Modified));
        assert!(hunks[0].diff_text.contains("println"));
        assert_eq!(hunks[0].language.as_deref(), Some("rust"));
    }
}
