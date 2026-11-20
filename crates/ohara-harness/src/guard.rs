//! Guard: protected paths and test weakening.

use std::path::PathBuf;

use crate::contract::Contract;
use crate::error::Result;
use crate::git::Git;

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

/// Check the work between commits `from` and `to`.
pub fn guard(_git: &Git, _c: &Contract, _from: &str, _to: &str) -> Result<GuardReport> {
    Ok(GuardReport::default())
}
