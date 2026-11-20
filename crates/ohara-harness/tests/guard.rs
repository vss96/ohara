//! Guard checks against a temporary git repository.

use ohara_harness::contract::Contract;
use ohara_harness::git::Git;
use ohara_harness::guard::{guard, Violation};
use std::path::PathBuf;

fn temp_repo() -> (tempfile::TempDir, Git) {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new(dir.path());
    git.run(&["init", "-q"]).unwrap();
    git.run(&["config", "user.name", "t"]).unwrap();
    git.run(&["config", "user.email", "t@example.com"]).unwrap();
    (dir, git)
}

fn contract(allow_removal: bool) -> Contract {
    let src = format!(
        r#"
outcome = "ship it"
protected_paths = ["Cargo.lock", "ci/**"]
[budget]
deadline = "2026-11-20T17:00:00Z"
[[script]]
name = "tests"
run = ["cargo", "test"]
kind = "test"
allow_test_removal = {allow_removal}
"#
    );
    Contract::parse(&src).unwrap()
}

fn write(dir: &tempfile::TempDir, rel: &str, body: &str) {
    let p = dir.path().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

const TESTS_FILE: &str =
    "#[test]\nfn a() { assert_eq!(1, 1); assert!(true); }\n#[test]\nfn b() { assert!(true); }\n";

/// Commit `before`, then `edit`, and return the guard violations.
fn run(c: &Contract, setup: &[(&str, &str)], edit: impl Fn(&tempfile::TempDir)) -> Vec<Violation> {
    let (dir, git) = temp_repo();
    for (p, b) in setup {
        write(&dir, p, b);
    }
    let from = git.commit_all("base").unwrap();
    edit(&dir);
    let to = git.commit_all("work").unwrap();
    guard(&git, c, &from, &to).unwrap().violations
}

#[test]
fn edit_to_protected_path_is_violation() {
    let v = run(&contract(false), &[("Cargo.lock", "1")], |d| {
        write(d, "Cargo.lock", "2")
    });
    assert_eq!(
        v,
        vec![Violation::ProtectedPathChanged("Cargo.lock".into())]
    );
}

#[test]
fn rename_of_protected_path_is_violation() {
    let v = run(&contract(false), &[("ci/a.yml", "1")], |d| {
        std::fs::rename(d.path().join("ci/a.yml"), d.path().join("b.yml")).unwrap()
    });
    assert!(v.contains(&Violation::ProtectedPathChanged("ci/a.yml".into())));
}

#[test]
fn contract_toml_is_implicitly_protected() {
    let v = run(
        &contract(false),
        &[("contract.toml", "a"), (".ohara/x/y", "a")],
        |d| {
            write(d, "contract.toml", "b");
            write(d, ".ohara/x/y", "b");
        },
    );
    assert_eq!(
        v,
        vec![
            Violation::ProtectedPathChanged(".ohara/x/y".into()),
            Violation::ProtectedPathChanged("contract.toml".into()),
        ]
    );
}

#[test]
fn deleting_test_file_is_violation() {
    let v = run(&contract(false), &[("tests/a.rs", TESTS_FILE)], |d| {
        std::fs::remove_file(d.path().join("tests/a.rs")).unwrap()
    });
    assert_eq!(v, vec![Violation::TestFileDeleted("tests/a.rs".into())]);
}

#[test]
fn adding_ignore_attribute_is_violation() {
    let ignored = TESTS_FILE.replacen("#[test]", "#[test]\n#[ignore]", 1);
    let v = run(&contract(true), &[("tests/a.rs", TESTS_FILE)], |d| {
        write(d, "tests/a.rs", &ignored)
    });
    assert_eq!(
        v,
        vec![Violation::TestsIgnored {
            file: PathBuf::from("tests/a.rs"),
            added: 1
        }]
    );
}

#[test]
fn removing_assertions_is_violation() {
    let weak = "#[test]\nfn a() {}\n#[test]\nfn b() {}\n";
    let v = run(&contract(false), &[("tests/a.rs", TESTS_FILE)], |d| {
        write(d, "tests/a.rs", weak)
    });
    assert_eq!(
        v,
        vec![Violation::AssertionsDropped {
            file: "tests/a.rs".into(),
            before: 3,
            after: 0
        }]
    );
}

#[test]
fn dropping_test_count_is_violation() {
    let one = "#[test]\nfn a() { assert!(true); assert!(true); assert!(true); }\n";
    let v = run(&contract(false), &[("tests/a.rs", TESTS_FILE)], |d| {
        write(d, "tests/a.rs", one)
    });
    assert_eq!(
        v,
        vec![Violation::TestCountDropped {
            file: "tests/a.rs".into(),
            before: 2,
            after: 1
        }]
    );
}

#[test]
fn allow_test_removal_suppresses_deletion_only() {
    let setup = [("tests/a.rs", TESTS_FILE), ("Cargo.lock", "1")];
    let v = run(&contract(true), &setup, |d| {
        std::fs::remove_file(d.path().join("tests/a.rs")).unwrap();
        write(d, "Cargo.lock", "2");
    });
    assert_eq!(
        v,
        vec![Violation::ProtectedPathChanged("Cargo.lock".into())]
    );
}

#[test]
fn adding_tests_is_clean() {
    let more = format!("{TESTS_FILE}#[test]\nfn c() {{ assert!(true); }}\n");
    let v = run(&contract(false), &[("tests/a.rs", TESTS_FILE)], |d| {
        write(d, "tests/a.rs", &more);
        write(d, "tests/new.rs", TESTS_FILE);
        write(d, "src/lib.rs", "pub fn f() {}");
    });
    assert!(v.is_empty(), "{v:?}");
}
