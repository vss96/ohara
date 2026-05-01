//! `git2::blame_file` wrapper: maps a contiguous, 1-based, inclusive
//! line range to one entry per distinct commit-of-origin.
//!
//! Plan 5 / Track A. Mirrors the `Repository: !Sync` pattern from
//! `GitWalker` / `GitCommitSource` — all blame work runs inside
//! `tokio::task::spawn_blocking` over an `Arc<Mutex<Repository>>` so
//! the tool stays usable from async callers.

use anyhow::{Context, Result};
use git2::Repository;
use ohara_core::explain::{BlameRange, BlameSource};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Blamer {
    repo_path: PathBuf,
    repo: Arc<Mutex<Repository>>,
}

impl Blamer {
    /// Open the repo at `path` (or any ancestor — uses
    /// `Repository::discover`). Failure to open is bubbled as an
    /// `anyhow::Error` so the CLI / MCP callers get a useful message.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let canonical = path.as_ref().to_path_buf();
        let repo = Repository::discover(&canonical).context("discover git repo")?;
        Ok(Self {
            repo_path: canonical,
            repo: Arc::new(Mutex::new(repo)),
        })
    }

    /// Visible to internal callers (the MCP server constructs one
    /// `Blamer` per session and reuses it).
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }
}

#[async_trait::async_trait]
impl BlameSource for Blamer {
    #[tracing::instrument(skip(self), fields(repo = %self.repo_path.display()))]
    async fn blame_range(
        &self,
        file: &str,
        line_start: u32,
        line_end: u32,
    ) -> ohara_core::Result<Vec<BlameRange>> {
        // git2::Repository is !Sync and `blame_file` is synchronous +
        // potentially expensive on long histories. Mirror the GitWalker
        // pattern: hop through Arc<Mutex<Repository>> + spawn_blocking
        // so we don't block the async runtime.
        let repo = self.repo.clone();
        let file = file.to_string();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<BlameRange>> {
            let guard = repo
                .lock()
                .map_err(|e| ohara_core::OhraError::Git(format!("repo lock poisoned: {e}")))?;
            blame_range_sync(&guard, &file, line_start, line_end)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }
}

/// Internal helper: synchronous core of `blame_range`. Lives in its
/// own free function so the async wrapper stays a thin layer over
/// `tokio::task::spawn_blocking`. Marked `pub(crate)` to keep it out
/// of the crate's public surface but still visible to the unit tests
/// in this module.
pub(crate) fn blame_range_sync(
    repo: &Repository,
    file: &str,
    line_start: u32,
    line_end: u32,
) -> Result<Vec<BlameRange>> {
    if line_start == 0 || line_end < line_start {
        return Ok(Vec::new());
    }

    // Clamp `line_end` to the file's actual length so `blame.get_line`
    // doesn't return None for every out-of-range line. The blame works
    // off the workdir checkout, matching `git blame`'s default semantic.
    let workdir = repo.workdir().context("repo has no workdir (bare repo?)")?;
    let on_disk = workdir.join(file);
    let line_count = match std::fs::read_to_string(&on_disk) {
        Ok(s) => count_lines(&s),
        Err(_) => 0,
    };
    if line_count == 0 {
        return Ok(Vec::new());
    }
    let end = line_end.min(line_count);
    if end < line_start {
        return Ok(Vec::new());
    }

    let blame = repo
        .blame_file(Path::new(file), None)
        .context("blame_file")?;

    // BTreeMap so the per-commit lines come out sorted, and final
    // iteration order is deterministic across runs (alphabetical by
    // SHA — recency ordering is the orchestrator's job, not blame's).
    let mut by_sha: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for line in line_start..=end {
        // git2's `get_line` is 1-based.
        if let Some(hunk) = blame.get_line(line as usize) {
            let sha = hunk.final_commit_id().to_string();
            by_sha.entry(sha).or_default().push(line);
        }
    }

    Ok(by_sha
        .into_iter()
        .map(|(commit_sha, lines)| BlameRange { commit_sha, lines })
        .collect())
}

fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|&b| b == b'\n').count() as u32;
    if s.ends_with('\n') {
        nl
    } else {
        nl + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;

    fn init_with_one_file(dir: &Path, lines: &[&str], commit_msg: &str) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        let body = lines.join("\n") + "\n";
        fs::write(dir.join("src.rs"), body).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("src.rs")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, commit_msg, &tree, &[])
                .unwrap();
        }
        repo
    }

    #[tokio::test]
    async fn blame_range_returns_one_commit_for_single_author_lines() {
        // Plan 5 / Task 5.r: a single-commit repo means every blamed line
        // resolves to the same SHA. Blamer::blame_range must collapse
        // that into exactly one BlameRange entry whose `lines` covers
        // the queried range.
        let dir = tempfile::tempdir().unwrap();
        init_with_one_file(
            dir.path(),
            &["fn one() {}", "fn two() {}", "fn three() {}"],
            "initial",
        );
        let blamer = Blamer::open(dir.path()).unwrap();
        let out = blamer.blame_range("src.rs", 1, 3).await.unwrap();
        assert_eq!(out.len(), 1, "single-author range collapses to one entry");
        assert_eq!(out[0].lines, vec![1, 2, 3]);
        assert!(!out[0].commit_sha.is_empty());
    }
}
