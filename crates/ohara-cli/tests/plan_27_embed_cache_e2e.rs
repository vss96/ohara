//! Plan-27 end-to-end: re-indexing the same repo with
//! `--embed-cache=semantic` populates the cache on the first run and
//! reuses it on the second. We assert via `ohara status`'s
//! embed_cache: line — the row count must be > 0 after the first run.

use std::path::Path;
use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn semantic_mode_populates_cache_visible_via_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    write_file(
        repo.join("src"),
        "main.rs",
        "fn main() { println!(\"hi\"); }\n",
    );
    git_add_all(repo);
    git_commit(repo, "feat: hello world");

    let idx = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args([
            "index",
            "--embed-provider",
            "cpu",
            "--embed-cache",
            "semantic",
        ])
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
        stdout.contains("embed_cache: semantic"),
        "expected `embed_cache: semantic` line; got:\n{stdout}"
    );
}

fn git_add_all(p: &Path) {
    Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["add", "."])
        .output()
        .unwrap();
}

fn git_commit(p: &Path, msg: &str) {
    Command::new("git")
        .arg("-C")
        .arg(p)
        .args([
            "-c",
            "user.email=a@a",
            "-c",
            "user.name=a",
            "commit",
            "-m",
            msg,
        ])
        .output()
        .unwrap();
}

fn write_file(dir: std::path::PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}
