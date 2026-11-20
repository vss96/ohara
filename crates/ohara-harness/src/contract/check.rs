//! Running contract scripts and reporting whether the contract is satisfied.

use std::path::{Path, PathBuf};

use ohara_core::task::{ContractHash, Deadline};

use super::{test_output, Contract, Script, ScriptKind};
use crate::agent::process::run_streaming;
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
    pub tests_run: Option<u32>,
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
///
/// Each script runs under `deadline` with its output teed to
/// `<logs>/<index>-<name>.log`. A `Test` script only passes when the output
/// shows enough tests ran and every required test passed.
pub async fn check(
    c: &Contract,
    expected: &ContractHash,
    cwd: &Path,
    deadline: Deadline,
    logs: &Path,
) -> Result<CheckReport> {
    if c.hash() != *expected {
        return Err(Error::Contract("contract hash mismatch".to_string()));
    }
    let mut scripts = Vec::with_capacity(c.scripts.len());
    for (i, script) in c.scripts.iter().enumerate() {
        scripts.push(run_script(i, script, cwd, deadline, logs).await?);
    }
    let satisfied = scripts.iter().all(|s| s.passed);
    Ok(CheckReport { scripts, satisfied })
}

async fn run_script(
    index: usize,
    script: &Script,
    cwd: &Path,
    deadline: Deadline,
    logs: &Path,
) -> Result<ScriptResult> {
    let log = logs.join(format!("{index}-{}.log", sanitize(&script.name)));
    // The transcript is append-only; start clean so output is this run's only.
    if let Err(e) = tokio::fs::remove_file(&log).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e.into());
        }
    }
    let exit = match run_streaming(&script.run, None, cwd, deadline, &log).await {
        Ok(exit) => exit,
        Err(e) => {
            return Ok(ScriptResult {
                name: script.name.clone(),
                passed: false,
                exit_code: None,
                tests_run: None,
                missing_required: Vec::new(),
                failures: vec![format!("could not run script: {e}")],
                log,
            })
        }
    };
    let mut failures = Vec::new();
    if exit.killed_at_deadline {
        failures.push("timed out at the deadline".to_string());
    }
    if !exit.killed_at_deadline && exit.code != Some(0) {
        failures.push(format!("exited with {:?}", exit.code));
    }
    let mut tests_run = None;
    let mut missing_required = Vec::new();
    if script.kind == ScriptKind::Test {
        let output = String::from_utf8_lossy(&tokio::fs::read(&log).await?).into_owned();
        let summary = test_output::parse(&output);
        tests_run = summary.ran;
        match summary.ran {
            None => failures.push("could not count tests".to_string()),
            Some(n) if n < script.effective_min_tests() => failures.push(format!(
                "ran {n} tests, expected at least {}",
                script.effective_min_tests()
            )),
            Some(_) => {}
        }
        missing_required = script
            .required_tests
            .iter()
            .filter(|req| !summary.passed_names.iter().any(|n| matches_name(n, req)))
            .cloned()
            .collect();
        for name in &missing_required {
            failures.push(format!("required test did not pass: {name}"));
        }
    }
    Ok(ScriptResult {
        name: script.name.clone(),
        passed: failures.is_empty(),
        exit_code: exit.code,
        tests_run,
        missing_required,
        failures,
        log,
    })
}

/// Whether a passed test satisfies a required name.
///
/// The canonical form is the cargo-printed test path. `required` matches when
/// it equals `passed` or is a suffix of it at a `::` boundary, so a bare
/// single-segment name matches that test in any module. A required name with
/// two or more segments that still fails is retried once without its first
/// segment, tolerating a crate/package prefix.
fn matches_name(passed: &str, required: &str) -> bool {
    if suffix_at_boundary(passed, required) {
        return true;
    }
    match required.split_once("::") {
        Some((_, rest)) => suffix_at_boundary(passed, rest),
        None => false,
    }
}

fn suffix_at_boundary(passed: &str, required: &str) -> bool {
    passed == required || passed.ends_with(&format!("::{required}"))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c.is_ascii_alphanumeric() || c == '-' {
            true => c,
            false => '_',
        })
        .collect()
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
        assert_eq!(r.scripts[0].tests_run, Some(0));
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
        assert!(r.scripts[0]
            .failures
            .iter()
            .any(|f| f.contains("a::absent")));
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

    #[test]
    fn crate_prefixed_required_name_matches() {
        assert!(matches_name(
            "contract::check::tests::t",
            "contract::check::tests::t"
        ));
        assert!(matches_name("contract::check::tests::t", "tests::t"));
        assert!(matches_name(
            "contract::check::tests::t",
            "harness::contract::check::tests::t"
        ));
        assert!(!matches_name(
            "other::tests::t",
            "harness::contract::check::tests::t"
        ));
    }

    #[test]
    fn required_suffix_respects_segment_boundary() {
        assert!(!matches_name("a::xfoo", "foo"));
        assert!(matches_name("a::foo", "foo"));
        assert!(matches_name("foo", "foo"));
    }

    #[tokio::test]
    async fn spawn_failure_is_an_unsatisfied_script_not_an_error() {
        let mut c = contract(ScriptKind::Check, "ok", &[]);
        c.scripts[0].run = vec!["/definitely/not/a/binary".into()];
        let r = run(&c).await;
        assert!(!r.satisfied);
        assert!(!r.scripts[0].failures.is_empty());
    }

    #[test]
    fn script_result_converts_to_core_record() {
        let r = ScriptResult {
            name: "tests".into(),
            passed: false,
            exit_code: Some(1),
            tests_run: Some(4),
            missing_required: vec!["a::b".into()],
            failures: vec!["boom".into()],
            log: PathBuf::from("logs/0-tests.log"),
        };
        let core = ohara_core::task::decision::ScriptResult::from(&r);
        assert_eq!(core.name, "tests");
        assert!(!core.passed);
        assert_eq!(core.exit_code, Some(1));
        assert_eq!(core.tests_run, Some(4));
        assert_eq!(core.missing_required, vec!["a::b".to_string()]);
        assert_eq!(core.log, PathBuf::from("logs/0-tests.log"));
    }
}
