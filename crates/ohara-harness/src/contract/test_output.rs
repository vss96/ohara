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
pub fn parse(_output: &str) -> TestSummary {
    TestSummary::default()
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
}
