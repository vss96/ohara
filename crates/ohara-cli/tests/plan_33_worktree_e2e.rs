//! Plan-33 end-to-end: a git clone and its linked worktrees share one
//! index. Binary-driven with an isolated `OHARA_HOME`, `--no-daemon` and
//! `--embed-provider cpu`. `#[ignore]`d like the other e2e tests because
//! indexing pulls the embedding model on first run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ohara(home: &Path, args: &[&str], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ohara"))
        .env("OHARA_HOME", home)
        .arg("--no-daemon")
        .args(args)
        .arg(path)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=a@a", "-c", "user.name=a"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, file: &str, body: &str, msg: &str) -> String {
    std::fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

fn init_main(dir: &Path) {
    git(dir, &["init", "-q"]);
    commit_file(dir, "a.rs", "fn alpha() {}\n", "add alpha");
    commit_file(dir, "b.rs", "fn beta() {}\n", "add beta");
    commit_file(dir, "c.rs", "fn gamma() {}\n", "add gamma");
}

/// Per-repo directories under `OHARA_HOME` (those holding an index db).
fn repo_dirs(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("index.sqlite").is_file())
        .collect()
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn worktree_shares_the_main_index_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    init_main(&main);

    // 1. index main.
    let o = ohara(
        home.path(),
        &["index", "--embed-provider", "cpu", "--no-progress"],
        &main,
    );
    assert!(o.status.success(), "index main: {}", stderr(&o));

    // 2. a linked worktree sees the same index.
    let wt = root.path().join("wt");
    git(
        &main,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "feat"],
    );
    let o = ohara(home.path(), &["status"], &wt);
    assert!(o.status.success(), "status wt: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("commits_behind_head: 0"), "status:\n{out}");
    assert!(out.contains("worktree: "), "status:\n{out}");
    assert_eq!(
        repo_dirs(home.path()).len(),
        1,
        "worktree must reuse the main clone's repo dir"
    );

    // 3. commit in the worktree and index it incrementally.
    let wt_sha = commit_file(&wt, "z.rs", "fn zebra() {}\n", "add zebra crossing guard");
    let o = ohara(
        home.path(),
        &[
            "index",
            "--incremental",
            "--embed-provider",
            "cpu",
            "--no-progress",
        ],
        &wt,
    );
    assert!(o.status.success(), "index wt: {}", stderr(&o));
    assert!(
        stdout(&o).contains("1 commit,"),
        "expected 1 new commit:\n{}",
        stdout(&o)
    );

    // 4. visibility: the worktree commit shows in the worktree only.
    let q = [
        "query",
        "--embed-provider",
        "cpu",
        "--query",
        "zebra crossing guard",
    ];
    let o = ohara(home.path(), &q, &wt);
    assert!(o.status.success(), "query wt: {}", stderr(&o));
    assert!(stdout(&o).contains(&wt_sha), "wt query:\n{}", stdout(&o));
    let o = ohara(home.path(), &q, &main);
    assert!(o.status.success(), "query main: {}", stderr(&o));
    assert!(!stdout(&o).contains(&wt_sha), "main query:\n{}", stdout(&o));

    // 5. main has nothing new to walk.
    let o = ohara(home.path(), &["index", "--incremental"], &main);
    assert!(o.status.success(), "index main again: {}", stderr(&o));
    assert!(
        stdout(&o).contains("index up-to-date at"),
        "main incremental:\n{}",
        stdout(&o)
    );

    // 6. explain from the worktree names the worktree commit.
    let o = ohara(home.path(), &["explain", "z.rs", "--lines", "1:1"], &wt);
    assert!(o.status.success(), "explain wt: {}", stderr(&o));
    assert!(stdout(&o).contains(&wt_sha), "explain:\n{}", stdout(&o));
}

#[test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
fn existing_index_survives_the_upgrade_without_reembedding() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    init_main(&main);

    let o = ohara(
        home.path(),
        &["index", "--embed-provider", "cpu", "--no-progress"],
        &main,
    );
    assert!(o.status.success(), "index: {}", stderr(&o));
    let before = repo_dirs(home.path());
    assert_eq!(before.len(), 1);
    let rows_before = commit_rows(&before[0].join("index.sqlite"));

    let o = ohara(home.path(), &["index", "--incremental"], &main);
    assert!(o.status.success(), "reindex: {}", stderr(&o));
    assert!(stdout(&o).contains("index up-to-date at"), "{}", stdout(&o));
    assert_eq!(repo_dirs(home.path()), before, "same repo dir");
    assert_eq!(
        commit_rows(&before[0].join("index.sqlite")),
        rows_before,
        "no re-embedding"
    );
}

/// Row count of `commit_record` via the sqlite3 CLI (no extra deps).
fn commit_rows(db: &Path) -> i64 {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg("SELECT COUNT(*) FROM commit_record;")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}
