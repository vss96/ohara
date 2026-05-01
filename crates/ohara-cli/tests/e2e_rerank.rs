//! Plan 3 / Track D — e2e: cross-encoder reranker picks the better-message
//! commit among near-duplicate hunks.
//!
//! Fixture: a synthetic repo with two commits that touch the same code in
//! the same way (similar diffs), but one commit message is on-topic for
//! the query and the other is generic. RRF puts both at the top by
//! similar dense + BM25 ranks; the reranker is what surfaces the
//! semantically-relevant message as #1.
//!
//! Gated `#[ignore]` like the other end-to-end tests because it
//! downloads the BGE-small embedding model AND the bge-reranker-base
//! cross-encoder on first run (~190 MB combined).

use git2::{Repository, Signature};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn make_commit(repo: &Repository, root: &Path, file: &str, body: &str, msg: &str) {
    std::fs::write(root.join(file), body).unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new(file)).unwrap();
    idx.write().unwrap();
    let oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let sig = Signature::now("Test", "test@example.com").unwrap();
    let parent_commit = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .and_then(|oid| repo.find_commit(oid).ok());
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .unwrap();
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore = "downloads embedding + reranker models on first run; opt in with --include-ignored"]
async fn cross_encoder_picks_better_message_among_near_duplicates() {
    let _g = env_lock();
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();

    // Commit 1: on-topic message, fits the query "retry with backoff".
    make_commit(
        &repo,
        repo_dir.path(),
        "retry.rs",
        "fn fetch() {\n    for _ in 0..3 {\n        if try_once() { return; }\n        sleep_backoff();\n    }\n}\n",
        "Add retry with exponential backoff to network fetch",
    );

    // Commit 2: generic, off-topic message. Identical hunk shape.
    make_commit(
        &repo,
        repo_dir.path(),
        "loop.rs",
        "fn poll() {\n    for _ in 0..3 {\n        if check_once() { return; }\n        wait_a_bit();\n    }\n}\n",
        "wip",
    );

    // Index the repo via the public CLI command path.
    let report = ohara_cli::commands::index::run(ohara_cli::commands::index::Args {
        path: repo_dir.path().to_path_buf(),
        incremental: false,
        force: false,
        commit_batch: 512,
        threads: 0,
        no_progress: true,
        profile: false,
        embed_provider: ohara_cli::commands::provider::ProviderArg::Auto,
    })
    .await
    .unwrap();
    assert_eq!(report.new_commits, 2, "fixture should have 2 commits");

    // Build a Retriever with the bge-reranker-base cross-encoder attached.
    use std::sync::Arc;
    let (repo_id, _, _) = ohara_cli::commands::resolve_repo_id(repo_dir.path()).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id).unwrap();
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db).await.unwrap());
    let embedder = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new)
            .await
            .unwrap()
            .unwrap(),
    );
    let reranker = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new)
            .await
            .unwrap()
            .unwrap(),
    );
    let retriever = ohara_core::Retriever::new(storage, embedder).with_reranker(reranker);

    let q = ohara_core::query::PatternQuery {
        query: "retry with exponential backoff".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: false,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await.unwrap();

    assert!(!hits.is_empty(), "rerank pipeline returned no hits");
    // The cross-encoder, given near-duplicate diffs, must lean on the
    // commit message + diff content combined to put the on-topic commit
    // first. The "wip" commit's message offers no semantic match for
    // "retry with exponential backoff".
    assert!(
        hits[0].commit_message.to_lowercase().contains("retry"),
        "rerank should pick the on-topic 'retry' commit first; got {:?}",
        hits.iter()
            .map(|h| h.commit_message.as_str())
            .collect::<Vec<_>>()
    );
}
