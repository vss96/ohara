//! The frozen task contract: model, canonical TOML, and hash.

use ohara_core::task::{ContractHash, Deadline};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Runs scripts against a worktree and reports whether the contract holds.
pub mod check;
/// Freezes the contract under a side ref and loads it back.
pub mod freeze;
/// Counts tests and names passed tests in script output.
pub mod test_output;

/// Resource limits for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// When the task must be done.
    pub deadline: Deadline,
    /// Spending cap in cents, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_ceiling_cents: Option<u64>,
}

/// What a script verifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    /// A test run: the output must show tests actually executed.
    Test,
    /// A plain check (lint, build): the exit code decides.
    Check,
}

/// One script the contract requires to pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    /// Display name.
    pub name: String,
    /// Command and arguments.
    pub run: Vec<String>,
    /// Test run or plain check.
    pub kind: ScriptKind,
    /// Tests that must appear as passed in the output.
    #[serde(default)]
    pub required_tests: Vec<String>,
    /// Minimum number of tests that must run (default 1 for `Test`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tests: Option<u32>,
    /// Whether the agent may reduce the number of tests.
    #[serde(default)]
    pub allow_test_removal: bool,
}

impl Script {
    /// The effective minimum test count: explicit, else 1 for tests, else 0.
    pub fn effective_min_tests(&self) -> u32 {
        match (self.min_tests, self.kind) {
            (Some(n), _) => n,
            (None, ScriptKind::Test) => 1,
            (None, ScriptKind::Check) => 0,
        }
    }
}

/// A rubric criterion. Phase 1 records and shows it; the judge scores it later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rubric {
    /// Stable id.
    pub id: String,
    /// What is being judged.
    pub criterion: String,
    /// Score needed to pass.
    pub pass_mark: u32,
    /// Top of the scale.
    pub scale_max: u32,
}

/// The task contract, serialised as `contract.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    /// The outcome the task must achieve.
    pub outcome: String,
    /// Paths agents may never change. The contract itself is implicitly protected.
    #[serde(default)]
    pub protected_paths: Vec<String>,
    /// Deadline and cost limits.
    pub budget: Budget,
    /// Scripts that must pass.
    #[serde(default, rename = "script")]
    pub scripts: Vec<Script>,
    /// Rubric criteria (recorded, not scored in phase 1).
    #[serde(default, rename = "rubric")]
    pub rubrics: Vec<Rubric>,
}

impl Contract {
    /// Parse a `contract.toml` document.
    pub fn parse(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Contract(format!("invalid contract: {e}")))
    }

    /// Canonical TOML: the struct re-serialised, never the raw input.
    pub fn to_toml(&self) -> String {
        // A plain struct of strings, numbers and vectors always serialises.
        toml::to_string(self).unwrap_or_default()
    }

    /// Blake3 of the canonical TOML.
    pub fn hash(&self) -> ContractHash {
        ContractHash(*blake3::hash(self.to_toml().as_bytes()).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
outcome = "ship it"
protected_paths = ["Cargo.lock"]
[budget]
deadline = "2026-11-20T17:00:00Z"
cost_ceiling_cents = 2000
[[script]]
name = "tests"
run = ["cargo", "test", "--workspace"]
kind = "test"
required_tests = ["a::b"]
[[rubric]]
id = "docs"
criterion = "documented"
pass_mark = 7
scale_max = 10
"#;

    #[test]
    fn contract_hash_is_stable_across_reparse() {
        let c = Contract::parse(SAMPLE).unwrap();
        let again = Contract::parse(&c.to_toml()).unwrap();
        assert_eq!(c, again);
        assert_eq!(c.hash(), again.hash());
        assert_eq!(c.to_toml(), again.to_toml());
        // Whitespace/comment differences in the input do not change the hash.
        let noisy = format!("# note\n\n{SAMPLE}\n\n");
        assert_eq!(Contract::parse(&noisy).unwrap().hash(), c.hash());
        assert_eq!(c.scripts[0].effective_min_tests(), 1);
    }
}
