//! Guard: protected paths and test weakening.

mod weakening;

use std::path::PathBuf;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::contract::Contract;
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

/// One path's content at both revisions. `None` means absent there.
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
/// `Git::diff_names` runs with `--no-renames`, so a rename or deletion lists
/// both the old and the new path and no extra old-path lookup is needed.
pub fn guard(git: &Git, c: &Contract, from: &str, to: &str) -> Result<GuardReport> {
    let protected = protected_set(c)?;
    let tests = TestFiles::new()?;
    let mut names = git.diff_names(from, to)?;
    names.sort();
    let mut files = Vec::new();
    for path in names {
        // Only test files need contents; skip the `git show` for the rest.
        let (before, after) = match tests.is_test_file(&path) {
            true => (show(git, from, &path), show(git, to, &path)),
            false => (None, None),
        };
        files.push(FileChange {
            path,
            before,
            after,
        });
    }
    Ok(evaluate(c, &protected, &tests, &files))
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
        if tests.is_test_file(&f.path) {
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
    for g in c
        .protected_paths
        .iter()
        .map(String::as_str)
        .chain(IMPLICIT_PROTECTED)
    {
        b.add(Glob::new(g).map_err(|e| Error::Guard(format!("protected path {g}: {e}")))?);
    }
    b.build()
        .map_err(|e| Error::Guard(format!("protected paths: {e}")))
}

/// File content at `rev`, or `None` when the path does not exist there.
fn show(git: &Git, rev: &str, path: &str) -> Option<String> {
    let spec = format!("{rev}:{path}");
    git.run(&["cat-file", "-e", &spec]).ok()?;
    git.run(&["show", &spec]).ok()
}
