use anyhow::Result;
use clap::{Parser, Subcommand};
use ohara_cli::commands;

#[derive(Parser, Debug)]
#[command(name = "ohara", version, about = "ohara — context lineage engine")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Install the ohara post-commit hook in a repo.
    Init(commands::init::Args),
    /// Build or update the index for a repo.
    Index(commands::index::Args),
    /// Run a debug pattern query against an indexed repo.
    Query(commands::query::Args),
    /// Print index status for a repo.
    Status(commands::status::Args),
    /// Explain why a file/range looks the way it does (Plan 5).
    Explain(commands::explain::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Init(a) => commands::init::run(a).await,
        Cmd::Index(a) => commands::index::run(a).await.map(|_| ()),
        Cmd::Query(a) => commands::query::run(a).await,
        Cmd::Status(a) => commands::status::run(a).await,
        Cmd::Explain(a) => commands::explain::run(a).await,
    }
}
