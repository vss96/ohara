use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::{Indexer, Storage};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, first_commit) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id);
    tracing::info!(repo = %canonical.display(), id = repo_id.as_str(), db = %db_path.display(), "indexing");

    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    storage.open_repo(&repo_id, &canonical.to_string_lossy(), &first_commit).await?;

    let embedder = Arc::new(tokio::task::spawn_blocking(|| {
        ohara_embed::FastEmbedProvider::new()
    }).await??);
    let commit_source = ohara_git::GitCommitSource::open(&canonical)?;
    let symbol_source = ohara_parse::GitSymbolSource::open(&canonical)?;

    let indexer = Indexer::new(storage.clone(), embedder.clone());
    let report = indexer.run(&repo_id, &commit_source, &symbol_source).await?;
    println!(
        "indexed: {} new commits, {} hunks, {} HEAD symbols",
        report.new_commits, report.new_hunks, report.head_symbols
    );
    Ok(())
}
