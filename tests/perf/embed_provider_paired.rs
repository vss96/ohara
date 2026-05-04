//! Plan 6 Task 3.3 — paired-flag quality gate for `--embed-provider`.
//!
//! Per RFC §Constraints (#7) the new perf flag must not change which
//! hunk wins rank-1 on the retry-pattern probe. We assert that twice:
//!
//!   * `find_pattern` with `--embed-provider cpu` (the explicit baseline).
//!   * `find_pattern` with `--embed-provider auto` (whatever the host
//!     would have picked anyway).
//!
//! Both halves drive the same code path that ships in the `ohara`
//! binary (`commands::index::run` + `Retriever::find_pattern`) so the
//! comparison is end-to-end, not unit-level.
//!
//! Run with:
//! ```sh
//! cargo test --workspace --release -- --include-ignored embed_provider
//! ```
//!
//! Both tests are `#[ignore]`'d because the embedder cold-starts BGE
//! (~80MB download on first run) — the same reason the rest of the
//! perf + e2e suite is gated.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ohara_cli::commands::provider::ProviderArg;
use ohara_embed::EmbedProvider;

/// Built/cached tiny fixture under `fixtures/tiny/repo`. Mirrors the
/// helper in `crates/ohara-cli/tests/e2e_find_pattern.rs` so the two
/// retry-pattern tests stay anchored to the same fixture.
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

/// Result of one retry-pattern lookup. The `(commit_sha, file_path)`
/// pair uniquely identifies the top hit on the tiny fixture; comparing
/// those is enough to catch a rank-1 swap without being brittle to
/// changes in storage-layer hunk-id allocation between runs. The
/// commit message tags along for human-readable assertion failures.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetryResult {
    commit_sha: String,
    commit_message: String,
    file_path: String,
}

/// Drive index + query for the retry-pattern probe under one provider arg.
/// Each call uses its own `OHARA_HOME` tempdir so the two halves of the
/// paired run never share state.
async fn run_retry_pattern(arg: ProviderArg) -> RetryResult {
    let repo = ensure_fixture();
    let home = tempfile::tempdir().expect("OHARA_HOME tempdir");
    std::env::set_var("OHARA_HOME", home.path());

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
        embed_provider: Some(arg),
        resources: ohara_cli::resources::ResourcesArg::Auto,
        embed_batch: None,
    };
    ohara_cli::commands::index::run(args)
        .await
        .expect("index run");

    let (repo_id, _, _) = ohara_cli::commands::resolve_repo_id(&repo).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id).unwrap();
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db).await.unwrap());
    // Resolve the same way `commands::query::run` does so the test
    // measures the *flag*, not a divergence in how `query` constructs
    // the embedder.
    let chosen = ohara_cli::commands::provider::resolve_provider(arg);
    let embedder = Arc::new(
        tokio::task::spawn_blocking(move || ohara_embed::FastEmbedProvider::with_provider(chosen))
            .await
            .unwrap()
            .expect("embedder boots under this provider"),
    );
    let retriever = ohara_core::Retriever::new(storage, embedder);

    let q = ohara_core::query::PatternQuery {
        query: "retry with exponential backoff".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true, // keep the probe stable: same retrieval pipeline both runs
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever
        .find_pattern(&repo_id, &q, now)
        .await
        .expect("find_pattern");
    assert!(!hits.is_empty(), "no hits under provider {:?}", arg);
    let top = &hits[0];
    RetryResult {
        commit_sha: top.commit_sha.clone(),
        commit_message: top.commit_message.clone(),
        file_path: top.file_path.clone(),
    }
}

/// Skip-with-warning helper: when `auto` resolves to a provider the
/// current build can't honour (CoreML, CUDA — pending Plan 6 Task 3.1
/// follow-up), we can't run the second half of the paired test. We
/// emit a `eprintln!` so opt-in runs see the skip in the log and
/// exit 0 instead of failing on `with_provider`.
fn auto_is_runnable() -> bool {
    matches!(
        ohara_cli::commands::provider::resolve_provider(ProviderArg::Auto),
        EmbedProvider::Cpu
    )
}

#[tokio::test]
#[ignore = "paired e2e — opt in via --include-ignored, downloads BGE on first run"]
async fn embed_provider_preserves_retry_pattern_rank_1() {
    if !auto_is_runnable() {
        eprintln!(
            "skipping embed_provider_preserves_retry_pattern_rank_1: \
             `--embed-provider auto` resolves to a non-CPU provider \
             that this build can't load yet (Plan 6 Task 3.1 follow-up)"
        );
        return;
    }
    let baseline = run_retry_pattern(ProviderArg::Cpu).await;
    let candidate = run_retry_pattern(ProviderArg::Auto).await;
    assert_eq!(
        baseline, candidate,
        "embed-provider flipped rank-1 on retry-pattern: \
         baseline={baseline:?}, candidate={candidate:?}"
    );
}

#[tokio::test]
#[ignore = "paired e2e — opt in via --include-ignored, downloads BGE on first run"]
async fn embed_provider_cpu_top_hit_is_retry_commit() {
    // Sanity gate matching the existing e2e: under the explicit CPU
    // provider the top hit must be the retry commit. This pins one
    // half of the paired comparison to a known-good state so a
    // future regression that broke *both* arms identically (e.g. an
    // index pipeline bug) can't pass the equality assertion above
    // by accident.
    let r = run_retry_pattern(ProviderArg::Cpu).await;
    assert!(
        r.commit_message.contains("retry"),
        "cpu top hit should be the retry commit, got: {r:?}"
    );
    assert!(
        !r.commit_message.contains("login"),
        "cpu top hit should NOT be the login commit: {r:?}"
    );
}
