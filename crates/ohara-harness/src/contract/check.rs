//! Running contract scripts and reporting whether the contract is satisfied.

use std::path::{Path, PathBuf};

use ohara_core::task::{ContractHash, Deadline};

use super::Contract;
use crate::error::{Error, Result};

/// Outcome of one script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptResult {
    /// Script name.
    pub name: String,
    /// True when every requirement held.
    pub passed: bool,
    /// Exit code, if the process exited normally.
    pub exit_code: Option<i32>,
    /// Tests counted in the output (test scripts only).
    pub tests_ran: Option<u32>,
    /// Required tests that did not pass.
    pub missing_required: Vec<String>,
    /// Why the script failed; empty when it passed.
    pub failures: Vec<String>,
    /// Combined output log.
    pub log: PathBuf,
}

/// Result of running every script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Per-script results, in contract order.
    pub scripts: Vec<ScriptResult>,
    /// True when all scripts passed.
    pub satisfied: bool,
}

/// Run the contract's scripts in `cwd` after verifying the contract hash.
pub async fn check(
    _c: &Contract,
    _expected: &ContractHash,
    _cwd: &Path,
    _deadline: Deadline,
    _logs: &Path,
) -> Result<CheckReport> {
    Err(Error::Contract("unimplemented".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Budget, Script, ScriptKind};
    use chrono::Utc;

    fn far() -> Deadline {
        Deadline(Utc::now() + chrono::Duration::seconds(60))
    }

    fn contract(kind: ScriptKind, output: &str, required: &[&str]) -> Contract {
        Contract {
            outcome: "x".into(),
            protected_paths: vec![],
            budget: Budget {
                deadline: far(),
                cost_ceiling_cents: None,
            },
            scripts: vec![Script {
                name: "tests".into(),
                run: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("printf '%s\\n' '{output}'"),
                ],
                kind,
                required_tests: required.iter().map(|s| s.to_string()).collect(),
                min_tests: None,
                allow_test_removal: false,
            }],
            rubrics: vec![],
        }
    }

    async fn run(c: &Contract) -> CheckReport {
        let dir = tempfile::tempdir().unwrap();
        check(c, &c.hash(), dir.path(), far(), &dir.path().join("logs"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn check_refuses_hash_mismatch() {
        let c = contract(ScriptKind::Check, "ok", &[]);
        let mut other = c.clone();
        other.outcome = "tampered".into();
        let dir = tempfile::tempdir().unwrap();
        let err = check(&other, &c.hash(), dir.path(), far(), dir.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("contract hash mismatch"), "{err}");
    }

    #[tokio::test]
    async fn zero_tests_executed_fails_the_script() {
        let c = contract(
            ScriptKind::Test,
            "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored",
            &[],
        );
        let r = run(&c).await;
        assert!(!r.satisfied);
        assert_eq!(r.scripts[0].tests_ran, Some(0));
    }

    #[tokio::test]
    async fn missing_required_test_fails_and_is_named() {
        let c = contract(
            ScriptKind::Test,
            "test a::present ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored",
            &["a::present", "a::absent"],
        );
        let r = run(&c).await;
        assert!(!r.satisfied);
        assert_eq!(r.scripts[0].missing_required, vec!["a::absent".to_string()]);
        assert!(r.scripts[0].failures.iter().any(|f| f.contains("a::absent")));
    }

    #[tokio::test]
    async fn unparseable_test_output_fails_test_script() {
        let c = contract(ScriptKind::Test, "all good, trust me", &[]);
        let r = run(&c).await;
        assert!(!r.satisfied);
        assert!(r.scripts[0]
            .failures
            .iter()
            .any(|f| f.contains("could not count tests")));
        // A plain check script with the same output passes on exit code alone.
        let c = contract(ScriptKind::Check, "all good, trust me", &[]);
        assert!(run(&c).await.satisfied);
    }
}
