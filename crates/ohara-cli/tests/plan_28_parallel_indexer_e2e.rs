//! Plan-28 e2e: --workers 4 indexes a fixture with N commits and
//! all rows persist correctly. The MAX(ulid) commit_sha matches HEAD.

use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn parallel_indexer_with_4_workers_indexes_all_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    for i in 0..10 {
        std::fs::write(repo.join(format!("f{i}.txt")), format!("content {i}\n")).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.email=a@a",
                "-c",
                "user.name=a",
                "commit",
                "-m",
                &format!("commit {i}"),
            ])
            .output()
            .unwrap();
    }
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let idx = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--workers", "4"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        idx.status.success(),
        "ohara index failed: {}",
        String::from_utf8_lossy(&idx.stderr)
    );

    let st = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .arg("status")
        .arg(repo)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&st.stdout);
    assert!(
        stdout.contains(&format!("last_indexed_commit: {head}")),
        "MAX(ulid) didn't match HEAD; status:\n{stdout}"
    );
}
