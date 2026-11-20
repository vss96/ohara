//! Thin shell-out wrapper over the `git` CLI.
//!
//! The harness writes refs and commits inside a task's own worktree, so it
//! uses the `git` binary directly (simple, and tests can drive a temp repo).
//! The API is synchronous so `reset_hard` fits the `Fn() -> Result<()>` reset
//! closure taken by the role runner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// Identity used for commits the harness itself creates.
const IDENTITY: [&str; 4] = [
    "-c",
    "user.name=ohara-harness",
    "-c",
    "user.email=ohara-harness@localhost",
];

/// Environment variables that would redirect git away from `cwd`.
const REDIRECT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

/// Git operations rooted at one working directory.
#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
}

impl Git {
    /// A runner executing `git` inside `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// The directory git commands run in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Run `git <args>` and return stdout. Non-zero exit is `Error::Git`.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.run_with_stdin(args, None)
    }

    /// Run `git <args>` feeding `stdin`, returning stdout.
    pub fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        let mut cmd = Command::new("git");
        for var in REDIRECT_ENV {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .args(args)
            .current_dir(&self.cwd)
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "git {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run `git <args>` with the harness commit identity configured.
    pub(crate) fn run_as_harness(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        let full: Vec<&str> = IDENTITY
            .iter()
            .copied()
            .chain(args.iter().copied())
            .collect();
        self.run_with_stdin(&full, stdin)
    }

    /// Fail unless `cwd` is itself the top level of a git work tree.
    ///
    /// Git searches parent directories, so a plain directory inside another
    /// repository would otherwise make mutating commands hit that repository.
    /// Every mutating helper calls this first.
    pub(crate) fn ensure_own_toplevel(&self) -> Result<()> {
        let top = self.run(&["rev-parse", "--show-toplevel"])?;
        let top = std::fs::canonicalize(top.trim())?;
        let cwd = std::fs::canonicalize(&self.cwd)?;
        if top != cwd {
            return Err(Error::Git(format!(
                "{} is not a git work tree root (nearest is {})",
                cwd.display(),
                top.display()
            )));
        }
        Ok(())
    }

    /// The commit `HEAD` points at.
    pub fn head(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// `git reset --hard <sha>`.
    pub fn reset_hard(&self, sha: &str) -> Result<()> {
        self.ensure_own_toplevel()?;
        self.run(&["reset", "--hard", sha]).map(|_| ())
    }

    /// Stage everything (`git add -A`, tracked and untracked) and commit it,
    /// returning the new `HEAD`. An empty change set still yields a commit.
    ///
    /// Safe only inside the task's own worktree (refused elsewhere): it sweeps
    /// up every file in the working directory. Hooks and commit signing are
    /// disabled: checkpoints are harness commits and must never block on user
    /// configuration.
    pub fn commit_all(&self, msg: &str) -> Result<String> {
        self.ensure_own_toplevel()?;
        self.run(&["add", "-A"])?;
        self.run_as_harness(
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--no-verify",
                "--allow-empty",
                "-m",
                msg,
            ],
            None,
        )?;
        self.head()
    }

    /// `git worktree add -b <branch> <path> <base>`.
    pub fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<()> {
        self.ensure_own_toplevel()?;
        let path = path.to_string_lossy();
        self.run(&["worktree", "add", "-b", branch, &path, base])
            .map(|_| ())
    }

    /// Paths changed between two commits.
    pub fn diff_names(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", "-z", "--no-renames", from, to])?;
        Ok(out
            .split('\0')
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, Git) {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).unwrap();
        git.run(&["config", "user.name", "t"]).unwrap();
        git.run(&["config", "user.email", "t@example.com"]).unwrap();
        (dir, git)
    }

    #[test]
    fn commit_all_head_diff_reset_and_worktree() {
        let (dir, git) = repo();
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        let first = git.commit_all("first").unwrap();
        assert_eq!(git.head().unwrap(), first);
        std::fs::write(dir.path().join("b.txt"), "2").unwrap();
        let second = git.commit_all("second").unwrap();
        assert_eq!(git.diff_names(&first, &second).unwrap(), vec!["b.txt"]);
        git.reset_hard(&first).unwrap();
        assert_eq!(git.head().unwrap(), first);
        assert!(!dir.path().join("b.txt").exists());

        let wt = dir.path().join("wt");
        git.worktree_add(&wt, "ohara/x", &first).unwrap();
        assert!(wt.join("a.txt").exists());
        assert!(git.run(&["bogus-subcommand"]).is_err());
    }

    #[test]
    fn commit_all_refuses_non_repo_subdir_of_outer_repo() {
        let (outer, outer_git) = repo();
        std::fs::write(outer.path().join("a.txt"), "1").unwrap();
        let before = outer_git.commit_all("first").unwrap();
        let sub = outer.path().join("plain");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("new.txt"), "x").unwrap();
        let inner = Git::new(&sub);
        assert!(inner.commit_all("nope").is_err());
        assert!(inner.reset_hard(&before).is_err());
        assert_eq!(outer_git.head().unwrap(), before);
        assert!(outer_git
            .run(&["status", "--porcelain"])
            .unwrap()
            .contains("plain"));
    }

    #[test]
    fn commit_all_skips_hooks_and_signing() {
        let (dir, git) = repo();
        let hook = dir.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git.run(&["config", "commit.gpgsign", "true"]).unwrap();
        git.run(&["config", "gpg.program", "false"]).unwrap();
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        assert!(git.commit_all("checkpoint").is_ok());
    }

    #[test]
    fn diff_names_is_nul_split_and_unrenamed() {
        let (dir, git) = repo();
        std::fs::write(dir.path().join("caf\u{e9} \"x\".txt"), "1").unwrap();
        let first = git.commit_all("first").unwrap();
        std::fs::rename(
            dir.path().join("caf\u{e9} \"x\".txt"),
            dir.path().join("b.txt"),
        )
        .unwrap();
        let second = git.commit_all("second").unwrap();
        let mut names = git.diff_names(&first, &second).unwrap();
        names.sort();
        assert_eq!(
            names,
            vec!["b.txt".to_string(), "caf\u{e9} \"x\".txt".to_string()]
        );
    }
}
