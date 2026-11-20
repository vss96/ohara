//! Test-weakening detection by token counting (no parsing).

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use super::Violation;
use crate::error::{Error, Result};

/// Globs identifying test files.
const TEST_GLOBS: [&str; 7] = [
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
const IGNORE_TOKENS: [&str; 5] = [
    "#[ignore",
    "@pytest.mark.skip",
    ".skip(",
    "xit(",
    "@Disabled",
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

    pub(super) fn is_test_file(&self, path: &str) -> bool {
        self.0.is_match(path)
    }
}

fn count(text: &str, tokens: &[&str]) -> u32 {
    let n: usize = tokens.iter().map(|t| text.matches(t).count()).sum();
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
    let (tb, ta) = (count(before, &TEST_TOKENS), count(after, &TEST_TOKENS));
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
        ] {
            assert!(t.is_test_file(p), "{p}");
        }
        for p in ["src/lib.rs", "contest.rs", "src/testing.rs"] {
            assert!(!t.is_test_file(p), "{p}");
        }
    }

    #[test]
    fn counts_tokens_per_language() {
        assert_eq!(count("it('a', () => { expect(1) })", &TEST_TOKENS), 1);
        assert_eq!(
            count("@Test void a() { assertEquals(1, 1); }", &ASSERT_TOKENS),
            2
        );
        assert_eq!(count("xit('a'); describe.skip(", &IGNORE_TOKENS), 2);
    }
}
