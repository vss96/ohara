use anyhow::Result;
use clap::{Parser, Subcommand};
use ohara_cli::commands;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// `cargo --version` style: "0.6.0-dev (c20597f)" so a local build is
/// distinguishable from a tagged release at a glance. `OHARA_GIT_SHA` is
/// injected by `build.rs`; "unknown" is the source-tarball fallback.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("OHARA_GIT_SHA"), ")");

#[derive(Parser, Debug)]
#[command(name = "ohara", version = VERSION, about = "ohara — context lineage engine")]
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
    /// Self-update the ohara binary by checking GitHub Releases.
    Update(commands::update::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Init(a) => commands::init::run(a).await,
        Cmd::Index(a) => commands::index::run(a).await.map(|_| ()),
        Cmd::Query(a) => commands::query::run(a).await,
        Cmd::Status(a) => commands::status::run(a).await,
        Cmd::Explain(a) => commands::explain::run(a).await,
        Cmd::Update(a) => commands::update::run(a).await,
    }
}

/// Install the global tracing subscriber.
///
/// The CLI layers two `tracing_subscriber::Layer`s on top of the registry:
///
/// 1. `IndicatifLayer` — owns a `MultiProgress` that pins progress bars to
///    the bottom of the terminal. Bars are driven by spans annotated via
///    `tracing_indicatif::span_ext::IndicatifSpanExt::pb_set_*` (see
///    `crate::progress::IndicatifProgress`).
/// 2. `fmt::Layer` — the human-readable log writer, but with its writer
///    redirected through `IndicatifLayer::get_stderr_writer()`. That writer
///    calls `MultiProgress::suspend(...)` for every line, so log lines
///    print *above* the progress bar instead of scrolling it away.
///
/// `EnvFilter` defaults to `info,ohara=debug` (override with `RUST_LOG`).
fn init_tracing() {
    let indicatif_layer = IndicatifLayer::new();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug"));
    let fmt_layer =
        tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(indicatif_layer)
        .init();
}
