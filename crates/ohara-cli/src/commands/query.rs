use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::query::PatternQuery;
use ohara_core::Retriever;
use std::path::PathBuf;
use std::sync::Arc;

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
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, _, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let embedder =
        Arc::new(tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new).await??);

    let retriever = Retriever::new(storage, embedder);
    let q = PatternQuery {
        query: args.query,
        k: args.k,
        language: args.language,
        since_unix: None,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await?;
    println!("{}", serde_json::to_string_pretty(&hits)?);
    Ok(())
}
