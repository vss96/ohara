//! End-to-end: build the fixture repo, index it, query for "retry", assert the
//! retry commit ranks first.

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
async fn find_pattern_returns_retry_commit_first() {
    let repo = ensure_fixture();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    // index
    let args = ohara_cli::commands::index::Args {
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
    };
    ohara_cli::commands::index::run(args).await.unwrap();

    // build a Retriever directly to avoid parsing CLI stdout
    let (repo_id, _, _) = ohara_cli::commands::resolve_repo_id(&repo).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id).unwrap();
    let storage = std::sync::Arc::new(ohara_storage::SqliteStorage::open(&db).await.unwrap());
    let embedder = std::sync::Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new)
            .await
            .unwrap()
            .unwrap(),
    );
    let retriever = ohara_core::Retriever::new(storage, embedder);

    let q = ohara_core::query::PatternQuery {
        query: "retry with exponential backoff".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await.unwrap();

    assert!(!hits.is_empty(), "no hits for 'retry'");
    assert!(
        hits[0].commit_message.contains("retry"),
        "top hit should be the retry commit, got: {}",
        hits[0].commit_message
    );
    // The unrelated login commit should not be the top result.
    assert!(
        !hits[0].commit_message.contains("login"),
        "top hit should NOT be the login commit"
    );
}
