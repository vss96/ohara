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
        let mut child = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
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
        let full: Vec<&str> = IDENTITY.iter().copied().chain(args.iter().copied()).collect();
        self.run_with_stdin(&full, stdin)
    }

    /// The commit `HEAD` points at.
    pub fn head(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// `git reset --hard <sha>`.
    pub fn reset_hard(&self, sha: &str) -> Result<()> {
        self.run(&["reset", "--hard", sha]).map(|_| ())
    }

    /// Stage everything (`git add -A`, tracked and untracked) and commit it,
    /// returning the new `HEAD`. An empty change set still yields a commit.
    ///
    /// Safe only inside the task's own worktree: it sweeps up every file in
    /// the working directory.
    pub fn commit_all(&self, msg: &str) -> Result<String> {
        self.run(&["add", "-A"])?;
        self.run_as_harness(&["commit", "--allow-empty", "-m", msg], None)?;
        self.head()
    }

    /// `git worktree add -b <branch> <path> <base>`.
    pub fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<()> {
        let path = path.to_string_lossy();
        self.run(&["worktree", "add", "-b", branch, &path, base])
            .map(|_| ())
    }

    /// Paths changed between two commits.
    pub fn diff_names(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", from, to])?;
        Ok(out.lines().map(str::to_string).collect())
    }
}
