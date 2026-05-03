use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::perf_trace::timed_phase;
use ohara_core::query::PatternQuery;
use ohara_core::Retriever;
use std::path::PathBuf;
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

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, _, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage =
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
    // Plan 3: cross-encoder rerank by default. Skip the model download
    // (and runtime cost) only when the caller explicitly passes
    // --no-rerank.
    let retriever = if args.no_rerank {
        Retriever::new(storage, embedder)
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
        Retriever::new(storage, embedder).with_reranker(reranker)
    };
    let q = PatternQuery {
        query: args.query,
        k: args.k,
        language: args.language,
        since_unix: None,
        no_rerank: args.no_rerank,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await?;
    println!("{}", serde_json::to_string_pretty(&hits)?);
    Ok(())
}
