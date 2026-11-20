//! Test-weakening detection by token counting (no parsing).

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use super::Violation;
use crate::error::{Error, Result};

/// Globs identifying test files.
const TEST_GLOBS: [&str; 11] = [
    "**/__tests__/**",
    "**/test/**",
    "**/*_spec.*",
    "**/tests.rs",
    "**/tests/**",
    "**/*_test.*",
    "**/*_tests.rs",
    "**/test_*.py",
    "**/*.test.*",
    "**/*.spec.*",
    "**/src/test/**",
];

const TEST_TOKENS: [&str; 6] = [
    "#[test]",
    "#[tokio::test]",
    "def test_",
    "it(",
    "test(",
    "@Test",
];
/// Markers that disable a test. Counts are approximate: overlapping forms
/// such as `@unittest.skip(` match two tokens, which only inflates `added`.
const IGNORE_TOKENS: [&str; 12] = [
    "#[ignore",
    "#[cfg(any())]",
    "pytest.mark.skip",
    "unittest.skip",
    ".skip(",
    "xit(",
    "xtest(",
    "xdescribe(",
    "@Ignore",
    "@Disabled",
    "t.Skip(",
    "t.Skipf(",
];
/// Disabled forms that also contain a live-test token (`it(`, `test(`);
/// subtracted from the test count so disabling can't offset a removal.
const DISABLED_TEST_TOKENS: [&str; 2] = ["xit(", "xtest("];
/// Unambiguous markers that make any file a test file (inline tests).
const CONTENT_MARKERS: [&str; 6] = [
    "#[test]",
    "#[cfg(test)]",
    "#[tokio::test]",
    "def test_",
    "@Test",
    "describe(",
];
const ASSERT_TOKENS: [&str; 3] = ["assert", "expect(", "assertEquals"];

/// Matches test files by path.
pub(super) struct TestFiles(GlobSet);

impl TestFiles {
    pub(super) fn new() -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for g in TEST_GLOBS {
            b.add(Glob::new(g).map_err(|e| Error::Guard(format!("test glob {g}: {e}")))?);
        }
        let set = b
            .build()
            .map_err(|e| Error::Guard(format!("test globs: {e}")))?;
        Ok(Self(set))
    }

    /// A test file by path, or by containing a test marker at either revision.
    pub(super) fn is_test_file(
        &self,
        path: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> bool {
        self.0.is_match(path)
            || [before, after]
                .into_iter()
                .flatten()
                .any(|t| CONTENT_MARKERS.iter().any(|m| t.contains(m)))
    }
}

fn test_count(text: &str) -> u32 {
    count(text, &TEST_TOKENS).saturating_sub(count(text, &DISABLED_TEST_TOKENS))
}

/// A single-line comment: `//`, or `#` that is not a Rust attribute or shebang.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || (t.starts_with('#') && !t.starts_with("#[") && !t.starts_with("#!"))
}

fn count(text: &str, tokens: &[&str]) -> u32 {
    let code = text
        .lines()
        .filter(|l| !is_comment_line(l))
        .collect::<Vec<_>>()
        .join("\n");
    let n: usize = tokens.iter().map(|t| code.matches(t).count()).sum();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Compare a test file's contents across revisions. `None` means absent.
/// `allow_removal` suppresses deletion and count/assertion drops, never ignores.
pub(super) fn compare(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
    allow_removal: bool,
) -> Vec<Violation> {
    let file = Path::new(path).to_path_buf();
    let Some(before) = before else {
        return added_ignores(file, 0, after);
    };
    let Some(after) = after else {
        return match allow_removal {
            true => vec![],
            false => vec![Violation::TestFileDeleted(file)],
        };
    };
    let mut out = added_ignores(file.clone(), count(before, &IGNORE_TOKENS), Some(after));
    if allow_removal {
        return out;
    }
    let (tb, ta) = (test_count(before), test_count(after));
    if ta < tb {
        out.push(Violation::TestCountDropped {
            file: file.clone(),
            before: tb,
            after: ta,
        });
    }
    let (ab, aa) = (count(before, &ASSERT_TOKENS), count(after, &ASSERT_TOKENS));
    if aa < ab {
        out.push(Violation::AssertionsDropped {
            file,
            before: ab,
            after: aa,
        });
    }
    out
}

fn added_ignores(file: std::path::PathBuf, before: u32, after: Option<&str>) -> Vec<Violation> {
    let Some(after) = after else {
        return vec![];
    };
    let now = count(after, &IGNORE_TOKENS);
    match now > before {
        true => vec![Violation::TestsIgnored {
            file,
            added: now - before,
        }],
        false => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_test_files() {
        let t = TestFiles::new().unwrap();
        for p in [
            "tests/a.rs",
            "crates/x/tests/a/b.rs",
            "a_test.go",
            "x/foo_tests.rs",
            "pkg/test_a.py",
            "web/a.test.ts",
            "web/a.spec.js",
            "app/src/test/java/A.java",
            "web/__tests__/a.js",
            "x/test/a.go",
            "a_spec.rb",
            "src/tests.rs",
        ] {
            assert!(t.is_test_file(p, None, None), "{p}");
        }
        for p in ["src/lib.rs", "contest.rs", "src/testing.rs"] {
            assert!(!t.is_test_file(p, None, None), "{p}");
        }
    }

    #[test]
    fn inline_markers_make_a_test_file() {
        let t = TestFiles::new().unwrap();
        assert!(t.is_test_file("src/lib.rs", Some("#[cfg(test)] mod t {}"), None));
        assert!(t.is_test_file("src/lib.rs", None, Some("#[test] fn a() {}")));
        assert!(!t.is_test_file("src/lib.rs", Some("fn a() {}"), Some("fn b() {}")));
    }

    #[test]
    fn counts_tokens_per_language() {
        assert_eq!(count("it('a', () => { expect(1) })", &TEST_TOKENS), 1);
        assert_eq!(
            count("@Test void a() { assertEquals(1, 1); }", &ASSERT_TOKENS),
            2
        );
        assert_eq!(count("xit('a'); describe.skip(", &IGNORE_TOKENS), 2);
        assert_eq!(test_count("xtest('a'); test('b')"), 1);
    }

    #[test]
    fn commenting_out_a_test_drops_the_count() {
        let rust_before = "#[test]\nfn a() {}\n";
        let rust_after = "// #[test]\nfn a() {}\n";
        let v = compare("t.rs", Some(rust_before), Some(rust_after), false);
        assert!(v
            .iter()
            .any(|x| matches!(x, Violation::TestCountDropped { .. })));
        let py_before = "def test_foo():\n    pass\n";
        let py_after = "# def test_foo():\n    pass\n";
        let v = compare("test_a.py", Some(py_before), Some(py_after), false);
        assert!(v
            .iter()
            .any(|x| matches!(x, Violation::TestCountDropped { .. })));
    }

    #[test]
    fn commenting_out_assertions_drops_the_count() {
        for (before, after) in [
            ("assert_eq!(a, b);\n", "// assert_eq!(a, b);\n"),
            ("assert x\n", "# assert x\n"),
        ] {
            let v = compare("t.rs", Some(before), Some(after), false);
            assert!(
                v.iter()
                    .any(|x| matches!(x, Violation::AssertionsDropped { .. })),
                "{after}"
            );
        }
    }

    #[test]
    fn attributes_and_shebangs_are_still_counted() {
        let src = "#[test]\n#[tokio::test]\n#[ignore]\n#!/bin/sh\n";
        assert_eq!(count(src, &TEST_TOKENS), 2);
        assert_eq!(count(src, &IGNORE_TOKENS), 1);
    }
}
