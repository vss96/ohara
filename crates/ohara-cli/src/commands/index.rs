use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::{Indexer, IndexerReport, Storage};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Skip indexing (and embedder init) when HEAD is already indexed.
    /// Used by the post-commit hook so empty re-indexes are nearly free.
    #[arg(long)]
    pub incremental: bool,
    /// Force a full re-walk of HEAD symbols even when the watermark
    /// already points at HEAD. Clears existing symbol rows first so the
    /// new AST sibling-merge chunker (Plan 3 / Track C) populates the
    /// index without duplicates. Mutually exclusive with `--incremental`
    /// (force wins if both are set).
    #[arg(long)]
    pub force: bool,
    /// Number of commits to batch per storage transaction. Smaller =
    /// less peak RAM and more frequent fsyncs; larger = faster but
    /// uses more memory. Default 512.
    #[arg(long, default_value_t = 512)]
    pub commit_batch: usize,
    /// Cap the number of threads used by the embedder's ONNX runtime.
    /// 0 (default) means "let ort decide" (typically = CPU count).
    /// Lower this to keep ohara from saturating a shared dev machine.
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
    /// Disable the progress bar even when stderr is a TTY. The indexer
    /// still emits `tracing::info!` events every 100 commits.
    #[arg(long)]
    pub no_progress: bool,
}

pub async fn run(args: Args) -> Result<IndexerReport> {
    let (repo_id, canonical, first_commit) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    tracing::info!(repo = %canonical.display(), id = repo_id.as_str(), db = %db_path.display(), "indexing");

    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    storage
        .open_repo(&repo_id, &canonical.to_string_lossy(), &first_commit)
        .await?;

    // --force: clear existing HEAD symbol rows so the v0.3 AST sibling-merge
    // chunker (Track C) can repopulate without duplicates. The watermark and
    // commit/hunk history are untouched — only HEAD-snapshot symbols are
    // re-extracted. `force` wins over `incremental`.
    if args.force {
        tracing::info!("force: clearing existing HEAD symbol rows");
        storage.clear_head_symbols(&repo_id).await?;
    }

    // Fast path: when --incremental is set and storage's last_indexed_commit
    // matches HEAD, return immediately without booting the FastEmbed model
    // (which costs ~hundreds of ms even when cached). This is what makes the
    // post-commit hook nearly free on no-op re-indexes.
    if args.incremental && !args.force {
        let st = storage.get_index_status(&repo_id).await?;
        let walker = ohara_git::GitWalker::open(&canonical)?;
        let head = walker.head_commit_sha()?;
        if st.last_indexed_commit.as_deref() == Some(head.as_str()) {
            tracing::info!(sha = %head, "incremental: index up-to-date, skipping embedder init");
            println!("index up-to-date at {head}");
            return Ok(IndexerReport {
                new_commits: 0,
                new_hunks: 0,
                head_symbols: 0,
            });
        }
    }

    // Apply --threads before the embedder loads so the ort runtime
    // picks up the cap. ort honors `OMP_NUM_THREADS` and
    // `RAYON_NUM_THREADS` for its parallel ops; setting both is the
    // simplest cross-version knob.
    if args.threads > 0 {
        let n = args.threads.to_string();
        std::env::set_var("OMP_NUM_THREADS", &n);
        std::env::set_var("RAYON_NUM_THREADS", &n);
        tracing::info!(threads = args.threads, "capping embedder threads");
    }

    let embedder =
        Arc::new(tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new).await??);
    let commit_source = ohara_git::GitCommitSource::open(&canonical)?;
    let symbol_source = ohara_parse::GitSymbolSource::open(&canonical)?;

    let progress: Arc<dyn ohara_core::ProgressSink> = if args.no_progress {
        Arc::new(ohara_core::NullProgress)
    } else {
        Arc::new(crate::progress::IndicatifProgress::new())
    };

    let indexer = Indexer::new(storage.clone(), embedder.clone())
        .with_batch_commits(args.commit_batch)
        .with_progress(progress);
    let report = indexer
        .run(&repo_id, &commit_source, &symbol_source)
        .await?;
    // Two-sink summary: human-readable on stdout, structured event on
    // stderr so log aggregators / CI watchdogs / a future `--json` flag
    // see the same numbers.
    tracing::info!(
        new_commits = report.new_commits,
        new_hunks = report.new_hunks,
        head_symbols = report.head_symbols,
        "indexed"
    );
    println!(
        "indexed: {} new commits, {} hunks, {} HEAD symbols",
        report.new_commits, report.new_hunks, report.head_symbols
    );
    Ok(report)
}
