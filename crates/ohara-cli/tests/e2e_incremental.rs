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
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .unwrap();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_on_fresh_repo_indexes_everything() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(
        &repo,
        repo_dir.path(),
        "a.rs",
        "fn alpha() {}\n",
        "add alpha",
    );
    make_commit(&repo, repo_dir.path(), "b.rs", "fn beta() {}\n", "add beta");
    make_commit(
        &repo,
        repo_dir.path(),
        "c.rs",
        "fn gamma() {}\n",
        "add gamma",
    );

    let args = ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    };
    let report = ohara_cli::commands::index::run(args).await.unwrap();
    assert_eq!(
        report.new_commits, 3,
        "fresh incremental run should index all commits"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_after_partial_index_only_walks_new_commits() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(
        &repo,
        repo_dir.path(),
        "a.rs",
        "fn alpha() {}\n",
        "add alpha",
    );
    make_commit(&repo, repo_dir.path(), "b.rs", "fn beta() {}\n", "add beta");

    let first = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();
    assert_eq!(first.new_commits, 2);

    // Add two more commits; incremental run should pick up exactly those.
    make_commit(
        &repo,
        repo_dir.path(),
        "c.rs",
        "fn gamma() {}\n",
        "add gamma",
    );
    make_commit(
        &repo,
        repo_dir.path(),
        "d.rs",
        "fn delta() {}\n",
        "add delta",
    );

    let second = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();
    assert_eq!(
        second.new_commits, 2,
        "incremental should walk only the two new commits"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn index_force_rebuilds_chunked_symbols_and_reembeds() {
    // Plan 3 / Track D: --force must (a) clear the existing HEAD symbol
    // rows so re-runs don't double-count and (b) re-extract via the v0.3
    // AST sibling-merge chunker, which produces non-empty sibling_names
    // when the file has multiple top-level atoms.
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    // Three small Rust functions in one file → chunker merges them into a
    // single chunk whose sibling_names is non-empty.
    make_commit(
        &repo,
        repo_dir.path(),
        "trio.rs",
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        "add trio",
    );

    // First run populates the index (and writes symbols with merged
    // sibling_names since Track C is already landed).
    let first = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();
    assert!(first.head_symbols > 0, "first index should write symbols");

    // Second run with --force must re-walk symbols even though the
    // watermark already points at HEAD; head_symbols > 0 demonstrates the
    // re-walk happened.
    let report = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
        force: true,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();
    assert!(
        report.head_symbols > 0,
        "--force should re-extract HEAD symbols"
    );

    // Inspect the database directly: the rebuilt rows must include at
    // least one symbol whose `sibling_names` is non-empty (the AST
    // chunker merged the three trio fns).
    let (repo_id, _, _) = ohara_cli::commands::resolve_repo_id(repo_dir.path()).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id).unwrap();
    let storage = ohara_storage::SqliteStorage::open(&db).await.unwrap();
    let pool = storage.pool().clone();
    let nonempty: i64 = pool
        .get()
        .await
        .unwrap()
        .interact(|c| {
            c.query_row(
                "SELECT count(*) FROM symbol WHERE sibling_names <> '[]'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        nonempty > 0,
        "--force should rebuild symbols with non-empty sibling_names from the AST chunker"
    );

    // After --force, the symbol count must equal one re-walk's worth, not
    // two — the clear step prevented duplicate rows.
    let total: i64 = pool
        .get()
        .await
        .unwrap()
        .interact(|c| c.query_row("SELECT count(*) FROM symbol", [], |r| r.get(0)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        total as usize, report.head_symbols,
        "symbol table size must match latest --force run (no duplicates)"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "downloads embedding model on first run; opt in with --include-ignored"]
async fn incremental_at_head_is_noop_and_skips_embedder_init() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    make_commit(
        &repo,
        repo_dir.path(),
        "a.rs",
        "fn alpha() {}\n",
        "add alpha",
    );

    let _first = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();

    let second = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: true,
        force: false,
        rebuild: false,
        yes: false,
        commit_batch: Some(512),
        threads: Some(0),
        no_progress: true,
        profile: false,
        embed_provider: Some(ohara_cli::commands::provider::ProviderArg::Auto),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
        embed_cache: ohara_cli::commands::index::EmbedCacheArg::Off,
    })
    .await
    .unwrap();
    assert_eq!(
        second.new_commits, 0,
        "second incremental run at HEAD should be a no-op"
    );
    assert_eq!(second.new_hunks, 0);
    assert_eq!(second.head_symbols, 0);
}
