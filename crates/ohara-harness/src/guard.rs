//! Guard: protected paths and test weakening.

mod weakening;

use std::path::PathBuf;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::contract::{normalize_glob, Contract};
use crate::error::{Error, Result};
use crate::git::Git;

use weakening::TestFiles;

/// Paths protected regardless of the contract's own list.
const IMPLICIT_PROTECTED: [&str; 2] = ["contract.toml", ".ohara/**"];

/// A rule the agent's work broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A protected path (or the contract itself) was changed.
    ProtectedPathChanged(PathBuf),
    /// A test file was deleted.
    TestFileDeleted(PathBuf),
    /// Ignore/skip markers were added to a test file.
    TestsIgnored {
        /// The test file.
        file: PathBuf,
        /// How many markers were added.
        added: u32,
    },
    /// A test file has fewer tests than before.
    TestCountDropped {
        /// The test file.
        file: PathBuf,
        /// Test tokens before.
        before: u32,
        /// Test tokens after.
        after: u32,
    },
    /// A test file has fewer assertions than before.
    AssertionsDropped {
        /// The test file.
        file: PathBuf,
        /// Assertion tokens before.
        before: u32,
        /// Assertion tokens after.
        after: u32,
    },
}

/// Result of a guard run. Empty means clean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardReport {
    /// Every violation found.
    pub violations: Vec<Violation>,
}

/// One path's content at both revisions. `None` means the path does not
/// exist at that revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repo-relative path.
    pub path: String,
    /// Content at `from`.
    pub before: Option<String>,
    /// Content at `to`.
    pub after: Option<String>,
}

/// Check the work between commits `from` and `to`.
///
/// Fails closed: whether a path exists at each revision comes from
/// `git diff --name-status` (authoritative), and any failure to read a blob
/// that should exist is an error, never a clean report. `--no-renames` makes a
/// rename or deletion list both the old and the new path.
pub fn guard(git: &Git, c: &Contract, from: &str, to: &str) -> Result<GuardReport> {
    let protected = protected_set(c)?;
    let tests = TestFiles::new()?;
    let out = git.run(&["diff", "--name-status", "-z", "--no-renames", from, to])?;
    let mut entries = parse_name_status(&out)?;
    entries.sort();
    let mut files = Vec::new();
    for (status, path) in entries {
        let before = match status {
            'A' => None,
            _ => Some(show(git, from, &path)?),
        };
        let after = match status {
            'D' => None,
            _ => Some(show(git, to, &path)?),
        };
        files.push(FileChange {
            path,
            before,
            after,
        });
    }
    Ok(evaluate(c, &protected, &tests, &files))
}

/// `(status, path)` pairs from `diff --name-status -z --no-renames`.
fn parse_name_status(out: &str) -> Result<Vec<(char, String)>> {
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    let mut entries = Vec::new();
    while let Some(status) = parts.next() {
        let path = parts
            .next()
            .ok_or_else(|| Error::Guard(format!("malformed diff output near {status:?}")))?;
        let letter = status.chars().next().unwrap_or('M');
        entries.push((letter, path.to_string()));
    }
    Ok(entries)
}

/// Pure core of [`guard`]: violations for already-gathered changes.
pub fn check(c: &Contract, files: &[FileChange]) -> Result<GuardReport> {
    Ok(evaluate(c, &protected_set(c)?, &TestFiles::new()?, files))
}

fn evaluate(
    c: &Contract,
    protected: &GlobSet,
    tests: &TestFiles,
    files: &[FileChange],
) -> GuardReport {
    let allow_removal = c.scripts.iter().any(|s| s.allow_test_removal);
    let mut violations = Vec::new();
    for f in files {
        if protected.is_match(&f.path) {
            violations.push(Violation::ProtectedPathChanged(PathBuf::from(&f.path)));
        }
        if tests.is_test_file(&f.path, f.before.as_deref(), f.after.as_deref()) {
            violations.extend(weakening::compare(
                &f.path,
                f.before.as_deref(),
                f.after.as_deref(),
                allow_removal,
            ));
        }
    }
    GuardReport { violations }
}

fn protected_set(c: &Contract) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    let patterns = c
        .protected_paths
        .iter()
        .map(|g| normalize_glob(g))
        .chain(IMPLICIT_PROTECTED.map(str::to_string));
    for g in patterns {
        // Each pattern also protects everything beneath it, so a bare
        // directory name covers its contents.
        for p in [g.clone(), format!("{g}/**")] {
            b.add(Glob::new(&p).map_err(|e| Error::Guard(format!("protected path {p}: {e}")))?);
        }
    }
    b.build()
        .map_err(|e| Error::Guard(format!("protected paths: {e}")))
}

/// File content at `rev`. Called only for paths git says exist there, so any
/// failure is an error.
fn show(git: &Git, rev: &str, path: &str) -> Result<String> {
    git.run(&["show", &format!("{rev}:{path}")])
}
