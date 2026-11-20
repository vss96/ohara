//! Counting tests and finding passed test names in script output.

use std::collections::BTreeSet;

/// What a test run's output revealed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSummary {
    /// Number of tests that passed, if a known format was recognised.
    pub ran: Option<u32>,
    /// Names of tests reported as passed.
    pub passed_names: BTreeSet<String>,
}

/// Parse cargo, pytest or jest/vitest output.
pub fn parse(output: &str) -> TestSummary {
    let mut passed_names = BTreeSet::new();
    let mut cargo_total: Option<u32> = None;
    let mut other_total: Option<u32> = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(name) = cargo_passed_name(line) {
            passed_names.insert(name.to_string());
            continue;
        }
        if let Some(name) = pytest_passed_name(line) {
            passed_names.insert(name.to_string());
            continue;
        }
        if line.starts_with("test result:") {
            let n = passed_count(line).unwrap_or(0);
            cargo_total = Some(cargo_total.unwrap_or(0) + n);
            continue;
        }
        if is_summary_line(line) {
            let n = passed_count(line).unwrap_or(0);
            other_total = Some(other_total.unwrap_or(0) + n);
        }
    }
    TestSummary {
        ran: cargo_total.or(other_total),
        passed_names,
    }
}

/// `test <name> ... ok` (cargo).
fn cargo_passed_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("test ")?;
    rest.strip_suffix(" ... ok")
}

/// `<path>::<name> PASSED [ 50%]` (pytest -v).
fn pytest_passed_name(line: &str) -> Option<&str> {
    let idx = line.find(" PASSED")?;
    let name = line[..idx].trim();
    name.contains("::").then_some(name)
}

/// Jest/vitest `Tests:`/`Tests ` summaries and pytest `... N passed ... in 1s` lines.
fn is_summary_line(line: &str) -> bool {
    line.starts_with("Tests:")
        || line.starts_with("Tests ")
        || (line.contains(" passed") && line.contains(" in "))
}

/// The number directly before the word `passed`.
fn passed_count(line: &str) -> Option<u32> {
    line.match_indices("passed").find_map(|(idx, _)| {
        line[..idx]
            .split_whitespace()
            .next_back()
            .and_then(|tok| tok.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_pytest_jest_counts_parse() {
        let cargo = "running 2 tests\ntest a::one ... ok\ntest a::two ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored\n\nrunning 1 test\ntest b::three ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n";
        let s = parse(cargo);
        assert_eq!(s.ran, Some(3));
        assert!(s.passed_names.contains("a::one"));
        assert!(s.passed_names.contains("b::three"));

        let zero = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored\n";
        assert_eq!(parse(zero).ran, Some(0));

        let pytest = "tests/test_x.py::test_a PASSED [ 50%]\ntests/test_x.py::test_b FAILED [100%]\n===== 1 failed, 4 passed in 0.12s =====\n";
        let s = parse(pytest);
        assert_eq!(s.ran, Some(4));
        assert!(s.passed_names.contains("tests/test_x.py::test_a"));
        assert!(!s.passed_names.contains("tests/test_x.py::test_b"));

        let jest = "Tests:       1 failed, 5 passed, 6 total\n";
        assert_eq!(parse(jest).ran, Some(5));
        let vitest = " Tests  7 passed (7)\n";
        assert_eq!(parse(vitest).ran, Some(7));

        assert_eq!(parse("hello world\n").ran, None);
    }

    #[test]
    fn multiple_pytest_summaries_are_summed() {
        let first_empty =
            "===== no tests ran, 0 passed in 0.01s =====\n===== 3 passed in 0.1s =====\n";
        assert_eq!(parse(first_empty).ran, Some(3));
        let two = "===== 2 passed in 0.1s =====\n===== 3 passed in 0.2s =====\n";
        assert_eq!(parse(two).ran, Some(5));
    }
}
