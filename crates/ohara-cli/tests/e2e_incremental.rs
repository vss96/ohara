//! End-to-end tests for `ohara index --incremental`.
//!
//! Each test boots a tempdir-backed git repo and a fresh OHARA_HOME so the
//! sqlite index is hermetic. They are gated on `--include-ignored` because
//! the first run downloads the FastEmbed model.

use git2::{Repository, Signature};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// All tests in this file mutate `OHARA_HOME` (a process-global env var).
/// `cargo test` runs them in parallel by default, so we serialize them
/// behind a single mutex.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn make_commit(repo: &Repository, dir: &Path, file: &str, body: &str, msg: &str) {
    fs::write(dir.join(file), body).unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(file)).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("a", "a@a").unwrap();
    let parents: Vec<git2::Commit> = match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(c) => vec![c],
        None => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs).unwrap();
}

#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_on_fresh_repo_indexes_everything() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(&repo, repo_dir.path(), "a.rs", "fn alpha() {}\n", "add alpha");
    make_commit(&repo, repo_dir.path(), "b.rs", "fn beta() {}\n", "add beta");
    make_commit(&repo, repo_dir.path(), "c.rs", "fn gamma() {}\n", "add gamma");

    let args = ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
    };
    let report = ohara_cli::commands::index::run(args).await.unwrap();
    assert_eq!(report.new_commits, 3, "fresh incremental run should index all commits");
}

#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_after_partial_index_only_walks_new_commits() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(&repo, repo_dir.path(), "a.rs", "fn alpha() {}\n", "add alpha");
    make_commit(&repo, repo_dir.path(), "b.rs", "fn beta() {}\n", "add beta");

    let first = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
    })
    .await
    .unwrap();
    assert_eq!(first.new_commits, 2);

    // Add two more commits; incremental run should pick up exactly those.
    make_commit(&repo, repo_dir.path(), "c.rs", "fn gamma() {}\n", "add gamma");
    make_commit(&repo, repo_dir.path(), "d.rs", "fn delta() {}\n", "add delta");

    let second = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
    })
    .await
    .unwrap();
    assert_eq!(second.new_commits, 2, "incremental should walk only the two new commits");
}

#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_at_head_is_noop_and_skips_embedder_init() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(&repo, repo_dir.path(), "a.rs", "fn alpha() {}\n", "add alpha");

    let _first = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
    })
    .await
    .unwrap();

    let second = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
    })
    .await
    .unwrap();
    assert_eq!(second.new_commits, 0, "second incremental run at HEAD should be a no-op");
    assert_eq!(second.new_hunks, 0);
    assert_eq!(second.head_symbols, 0);
}
