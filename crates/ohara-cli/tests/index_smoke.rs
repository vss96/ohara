// integration-style smoke test that calls into commands::index::run
// using a temp repo and OHARA_HOME pointing to a temp dir.

use git2::{Repository, Signature};
use std::fs;

#[tokio::test]
#[ignore = "requires network for first-time fastembed model download"]
async fn smoke_index_then_status() {
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    let sig = Signature::now("a", "a@a").unwrap();
    fs::write(repo_dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(std::path::Path::new("a.rs")).unwrap();
    idx.write().unwrap();
    let t = idx.write_tree().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "init",
        &repo.find_tree(t).unwrap(),
        &[],
    )
    .unwrap();

    let args = ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
        force: false,
        commit_batch: 512,
        threads: 0,
        no_progress: true,
        profile: false,
        embed_provider: ohara_cli::commands::provider::ProviderArg::Auto,
    };
    ohara_cli::commands::index::run(args).await.unwrap();

    // Note: Task 15 stubs status.rs; status::run currently returns an error.
    // Task 16 will implement it for real. For now, just verify the index call succeeded.
    // (When Task 16 lands, this test should be expanded to actually call status::run.)
}
