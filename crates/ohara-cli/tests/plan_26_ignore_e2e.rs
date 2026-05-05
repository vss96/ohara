//! Plan-26 end-to-end: `.oharaignore` causes the indexer to skip
//! ignored paths while keeping real source hunks.

use std::path::Path;
use std::process::Command;

fn ohara_bin() -> String {
    env!("CARGO_BIN_EXE_ohara").to_string()
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn mixed_commit_with_vendor_ignored_indexes_only_real_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let home = tempfile::tempdir().expect("ohara_home tempdir");

    // Build a minimal repo: one commit touching `src/main.rs` + `vendor/foo.c`.
    git_init(repo);
    write_file(repo.join("src"), "main.rs", "fn main() {}\n");
    write_file(
        repo.join("vendor"),
        "foo.c",
        "int main(void) { return 0; }\n",
    );
    git_add_all(repo);
    git_commit(repo, "feat: add main + vendor stub");

    // Write `.oharaignore`.
    std::fs::write(repo.join(".oharaignore"), "vendor/\n").unwrap();

    // Run `ohara index` against the fixture.
    // --embed-provider cpu: avoids CoreML auto-selection on Apple Silicon
    //   in debug builds where CoreML is not compiled in.
    // OHARA_HOME: points index DB to tempdir so tests don't pollute ~/.ohara.
    let out = Command::new(ohara_bin())
        .env("OHARA_HOME", home.path())
        .arg("index")
        .args(["--embed-provider", "cpu"])
        .arg(repo)
        .output()
        .expect("run ohara index");
    assert!(
        out.status.success(),
        "ohara index failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    // Run `ohara query` for a vendor-specific token; expect no vendor hit.
    // --no-rerank avoids loading the reranker model (speeds up the test).
    let q = Command::new(ohara_bin())
        .env("OHARA_HOME", home.path())
        .args([
            "query",
            "--query",
            "return 0",
            "--embed-provider",
            "cpu",
            "--no-rerank",
        ])
        .arg(repo)
        .output()
        .expect("run ohara query");
    let stdout = String::from_utf8_lossy(&q.stdout);
    assert!(
        !stdout.contains("vendor/foo.c"),
        "vendor path leaked into query results: {stdout}"
    );
}

fn git_init(p: &Path) {
    Command::new("git")
        .arg("init")
        .arg(p)
        .output()
        .expect("git init");
}

fn git_add_all(p: &Path) {
    Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["add", "."])
        .output()
        .expect("git add");
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
        .expect("git commit");
}

fn write_file(dir: std::path::PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}
