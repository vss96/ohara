//! End-to-end: build the fixture repo, index it, run `explain` against
//! the retry block, assert the retry commit shows up first.
//!
//! Plan 5 / Task 9. `#[ignore]`'d because indexing the fixture pulls
//! the FastEmbed model on first run (same constraint as
//! `e2e_find_pattern`).

use std::path::PathBuf;
use std::process::Command;

fn ensure_fixture() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let script = workspace.join("fixtures/build_tiny.sh");
    let repo = workspace.join("fixtures/tiny/repo");
    if !repo.join(".git").exists() {
        let s = Command::new("bash")
            .arg(&script)
            .status()
            .expect("run fixture script");
        assert!(s.success(), "fixture script failed");
    }
    repo
}

#[tokio::test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
async fn explain_e2e_returns_retry_commit_for_retry_lines() {
    // The fixture's `src.rs` ends up with the retry block (lines 2-6 in
    // the final HEAD checkout) added by the second commit. Asking
    // explain for that range must return the retry commit at hits[0].
    let repo = ensure_fixture();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    // Index first.
    let index_args = ohara_cli::commands::index::Args {
        path: repo.clone(),
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
    };
    ohara_cli::commands::index::run(index_args).await.unwrap();

    // Build storage + blamer directly to avoid parsing CLI stdout.
    let (repo_id, canonical, _) = ohara_cli::commands::resolve_repo_id(&repo).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id).unwrap();
    let storage = std::sync::Arc::new(ohara_storage::SqliteStorage::open(&db).await.unwrap());
    let blamer = ohara_git::Blamer::open(&canonical).unwrap();

    let q = ohara_core::explain::ExplainQuery {
        file: "src.rs".into(),
        line_start: 2,
        line_end: 6,
        k: 5,
        include_diff: true,
        include_related: false,
    };
    let (hits, meta) = ohara_core::explain::explain_change(storage.as_ref(), &blamer, &repo_id, &q)
        .await
        .unwrap();

    assert!(!hits.is_empty(), "explain should return at least one hit");
    assert!(
        hits[0].commit_message.contains("retry"),
        "top hit should be the retry commit, got: {:?}",
        hits[0].commit_message
    );
    assert!(
        matches!(hits[0].provenance, ohara_core::types::Provenance::Exact),
        "explain hits must always be Provenance::Exact"
    );
    assert!(meta.commits_unique >= 1);
}
