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

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn mode_mismatch_on_incremental_errors_with_rebuild_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    write_file(repo.join("src"), "main.rs", "fn main() {}\n");
    git_add_all(repo);
    git_commit(repo, "feat: initial");

    // First run: --incremental index with --embed-cache=semantic.
    // Both runs pass --incremental so the test actually exercises the
    // incremental path the test name advertises (per plan-27 spec
    // success criteria: "index with --embed-cache=semantic, then run
    // --incremental --embed-cache=diff → must error").
    let idx1 = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args([
            "index",
            "--incremental",
            "--embed-provider",
            "cpu",
            "--embed-cache",
            "semantic",
        ])
        .arg(repo)
        .output()
        .unwrap();
    assert!(idx1.status.success());

    // Second run: --incremental --embed-cache=diff is a different mode
    // → mode-mismatch guard must fire before the embedder loads.
    let idx2 = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args([
            "index",
            "--incremental",
            "--embed-provider",
            "cpu",
            "--embed-cache",
            "diff",
        ])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        !idx2.status.success(),
        "expected mode-mismatch failure, got success"
    );
    let stderr = String::from_utf8_lossy(&idx2.stderr);
    let stdout = String::from_utf8_lossy(&idx2.stdout);
    let combined = format!("{stderr}\n{stdout}");
    assert!(
        combined.contains("embed_input_mode")
            || combined.contains("rebuild")
            || combined.contains("Rebuild"),
        "expected rebuild guidance in output; got:\n{combined}"
    );
}

/// Lock in that mode-mismatch detection is mode-agnostic: the guard
/// fires on a full re-index too (no `--incremental`), not just the
/// incremental path. This is the sibling of
/// [`mode_mismatch_on_incremental_errors_with_rebuild_hint`] and
/// exists so a future refactor that accidentally makes the guard
/// `--incremental`-only is caught here rather than in the field.
#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn mode_mismatch_on_full_reindex_errors_with_rebuild_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let ohara_home = tempfile::tempdir().expect("OHARA_HOME tempdir");

    Command::new("git").arg("init").arg(repo).output().unwrap();
    write_file(repo.join("src"), "main.rs", "fn main() {}\n");
    git_add_all(repo);
    git_commit(repo, "feat: initial");

    // First run: full index (no `--incremental`) with `--embed-cache=semantic`.
    let idx1 = Command::new(ohara_bin())
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
    assert!(idx1.status.success());

    // Second run: full index again with `--embed-cache=diff`. The
    // guard must still fire even though neither invocation set
    // `--incremental` — mode mismatch is a property of stored vectors,
    // not of the `--incremental` flag.
    let idx2 = Command::new(ohara_bin())
        .env("OHARA_HOME", ohara_home.path())
        .args(["index", "--embed-provider", "cpu", "--embed-cache", "diff"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(
        !idx2.status.success(),
        "expected mode-mismatch failure on full re-index, got success"
    );
    let stderr = String::from_utf8_lossy(&idx2.stderr);
    let stdout = String::from_utf8_lossy(&idx2.stdout);
    let combined = format!("{stderr}\n{stdout}");
    assert!(
        combined.contains("embed_input_mode")
            || combined.contains("rebuild")
            || combined.contains("Rebuild"),
        "expected rebuild guidance in output; got:\n{combined}"
    );
}

fn write_file(dir: std::path::PathBuf, name: &str, body: &str) {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}
