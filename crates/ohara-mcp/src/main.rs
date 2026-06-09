use anyhow::Result;
use clap::Parser;
use ohara_mcp::server;
use std::path::PathBuf;

/// Daemon-mode flags. Must accept exactly what
/// `ohara_engine::client::spawn::spawn_daemon` passes, since the thin
/// MCP client spawns `current_exe() serve …`.
#[derive(Parser, Debug)]
#[command(name = "ohara-mcp serve")]
struct ServeCli {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    pid_file: PathBuf,
    #[arg(long)]
    readiness_file: PathBuf,
    /// Exit after this many seconds with no requests. 0 disables.
    #[arg(long, default_value_t = 1800)]
    idle_timeout: u64,
    #[arg(long)]
    registry_path: Option<PathBuf>,
    /// Drop the reranker session after this many idle seconds. 0 disables.
    #[arg(long, default_value_t = 600)]
    reranker_idle_secs: u64,
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

    // "serve" is a reserved first argument (used by spawn_daemon when the
    // thin client spawns current_exe as the daemon). MCP clients invoke
    // this binary with no args; never document "serve" as a user-facing flag.
    if std::env::args().nth(1).as_deref() == Some("serve") {
        return run_serve().await;
    }

    let workdir = std::env::current_dir()?;
    let server = server::OharaServer::open(workdir)?;
    server.serve_stdio().await
}

async fn run_serve() -> Result<()> {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| "ohara-mcp".into());
    // Drop the literal "serve" so clap sees `argv0 --flags…`.
    let rest = std::env::args_os().skip(2);
    let cli = ServeCli::try_parse_from(std::iter::once(argv0).chain(rest))
        .map_err(|e| anyhow::anyhow!("serve args: {e}"))?;
    ohara_engine::daemon::run_daemon(ohara_engine::daemon::DaemonOptions {
        socket: cli.socket,
        pid_file: cli.pid_file,
        readiness_file: cli.readiness_file,
        idle_timeout_secs: cli.idle_timeout,
        registry_path: cli.registry_path,
        reranker_idle_secs: cli.reranker_idle_secs,
    })
    .await
    .map_err(|e| anyhow::anyhow!("daemon: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn serve_cli_parses_the_flag_shape_spawn_daemon_uses() {
        // Mirrors ohara-engine client/spawn.rs: socket, pid-file,
        // readiness-file, registry-path; idle flags default.
        let cli = ServeCli::try_parse_from([
            "ohara-mcp",
            "--socket",
            "/tmp/o.sock",
            "--pid-file",
            "/tmp/o.pid",
            "--readiness-file",
            "/tmp/o.ready",
            "--registry-path",
            "/tmp/registry.json",
        ])
        .unwrap();
        assert_eq!(cli.idle_timeout, 1800);
        assert_eq!(cli.reranker_idle_secs, 600);
        assert!(cli.registry_path.is_some());
    }
}
