//! Freeze/load round trip against a temporary git repository.

use ohara_core::task::TaskId;
use ohara_harness::contract::freeze::{freeze, load_frozen};
use ohara_harness::contract::Contract;
use ohara_harness::git::Git;

fn temp_repo() -> (tempfile::TempDir, Git) {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new(dir.path());
    git.run(&["init", "-q"]).unwrap();
    git.run(&["config", "user.name", "t"]).unwrap();
    git.run(&["config", "user.email", "t@example.com"]).unwrap();
    (dir, git)
}

const SAMPLE: &str = r#"
outcome = "ship it"
protected_paths = ["Cargo.lock"]
[budget]
deadline = "2026-11-20T17:00:00Z"
[[script]]
name = "tests"
run = ["cargo", "test"]
kind = "test"
"#;

#[test]
fn freeze_then_load_round_trips() {
    let (_dir, git) = temp_repo();
    let task = TaskId::new();
    let c = Contract::parse(SAMPLE).unwrap();
    let hash = freeze(&git, &task, &c).unwrap();
    assert_eq!(hash, c.hash());
    let (loaded, loaded_hash) = load_frozen(&git, &task).unwrap();
    assert_eq!(loaded, c);
    assert_eq!(loaded_hash, hash);
    // The ref lives under the side namespace and the working tree is untouched.
    let refname = format!("refs/ohara/tasks/{task}/contract");
    assert!(git.run(&["rev-parse", "--verify", &refname]).is_ok());
    assert!(git.run(&["status", "--porcelain"]).unwrap().is_empty());
}
