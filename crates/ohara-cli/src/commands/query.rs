use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::perf_trace::timed_phase;
use ohara_core::query::PatternQuery;
use ohara_core::Retriever;
use ohara_engine::client::{find_or_spawn_daemon, registry_path, try_daemon_call};
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::FindPatternResult;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::provider::{resolve_provider, ProviderArg};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// The natural-language query
    #[arg(short, long)]
    pub query: String,
    #[arg(short, long, default_value_t = 5)]
    pub k: u8,
    #[arg(long)]
    pub language: Option<String>,
    /// Skip the cross-encoder rerank stage (faster, slightly less precise).
    #[arg(long)]
    pub no_rerank: bool,
    /// ONNX execution provider for the embedder + reranker. `auto`
    /// (default) follows the same heuristic as `ohara index`. See
    /// `commands::index::Args::embed_provider` for the current
    /// CoreML / CUDA support story.
    #[arg(long, value_enum, default_value_t = ProviderArg::Auto)]
    pub embed_provider: ProviderArg,
}

/// Run the retrieval engine in-process (standalone path).
///
/// Constructs storage, embedder, and optionally a reranker, then calls
/// [`Retriever::find_pattern_with_profile`]. Used both as the direct standalone
/// path and as the fallback when the daemon is unavailable or disabled.
async fn run_standalone(
    canonical: &Path,
    q: PatternQuery,
    args: &Args,
) -> Result<FindPatternResult> {
    let (repo_id, _, _) = super::resolve_repo_id(canonical)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage: Arc<dyn ohara_core::Storage> =
        Arc::new(timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?);
    let chosen_provider = resolve_provider(args.embed_provider);
    tracing::info!(provider = ?chosen_provider, "embedder");
    let embedder = Arc::new(
        timed_phase(
            "embed_load",
            tokio::task::spawn_blocking(move || {
                ohara_embed::FastEmbedProvider::with_provider(chosen_provider)
            }),
        )
        .await??,
    );
    let retriever = if q.no_rerank {
        Retriever::new(storage.clone(), embedder)
    } else {
        let reranker = Arc::new(
            timed_phase(
                "rerank_load",
                tokio::task::spawn_blocking(move || {
                    ohara_embed::FastEmbedReranker::with_provider(chosen_provider)
                }),
            )
            .await??,
        );
        Retriever::new(storage.clone(), embedder).with_reranker(reranker)
    };
    let now = chrono::Utc::now().timestamp();
    let (hits, _profile) = retriever
        .find_pattern_with_profile(&repo_id, &q, now)
        .await?;
    let behind = ohara_git::GitCommitsBehind::open(canonical)
        .map_err(|e| anyhow::anyhow!("commits_behind: {e}"))?;
    let index_status = ohara_core::query::compute_index_status(storage.as_ref(), &repo_id, &behind)
        .await
        .map_err(|e| anyhow::anyhow!("index_status: {e}"))?;
    let meta = ohara_core::query::ResponseMeta {
        index_status,
        hint: None,
        compatibility: None,
    };
    Ok(FindPatternResult { hits, meta })
}

pub async fn run(args: Args, no_daemon: bool) -> Result<()> {
    let canonical = std::fs::canonicalize(&args.path)
        .map_err(|e| anyhow::anyhow!("canonicalize {}: {e}", args.path.display()))?;

    let pattern_query = PatternQuery {
        query: args.query.clone(),
        k: args.k,
        language: args.language.clone(),
        since_unix: None,
        no_rerank: args.no_rerank,
    };

    let req = Request {
        id: 1,
        repo_path: Some(canonical.to_string_lossy().to_string()),
        method: RequestMethod::FindPattern(pattern_query.clone()),
    };

    let registry = registry_path().map_err(|e| anyhow::anyhow!("registry_path: {e}"))?;
    let current_exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;

    let daemon_resp = try_daemon_call(
        move || {
            find_or_spawn_daemon(
                &current_exe,
                env!("CARGO_PKG_VERSION"),
                option_env!("OHARA_GIT_SHA").unwrap_or("unknown"),
                &registry,
                no_daemon,
            )
        },
        req,
    )
    .await;

    let result: FindPatternResult = match daemon_resp {
        Some(resp) if resp.error.is_none() => {
            let value = resp
                .result
                .ok_or_else(|| anyhow::anyhow!("daemon response missing result"))?;
            serde_json::from_value(value)
                .map_err(|e| anyhow::anyhow!("decode FindPatternResult: {e}"))?
        }
        _ => run_standalone(&canonical, pattern_query, &args).await?,
    };

    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
