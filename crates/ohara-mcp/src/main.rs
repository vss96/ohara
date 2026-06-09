use anyhow::Result;
use ohara_mcp::server;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug")),
        )
        .with_writer(std::io::stderr)
        .init();
    let workdir = std::env::current_dir()?;
    let server = server::OharaServer::open(workdir).await?;
    server.serve_stdio().await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn serve_cli_parses_the_flag_shape_spawn_daemon_uses() {
        // Mirrors ohara-engine client/spawn.rs: socket, pid-file,
        // readiness-file, registry-path; idle flags default.
        let cli = ServeCli::try_parse_from([
            "ohara-mcp",
            "--socket", "/tmp/o.sock",
            "--pid-file", "/tmp/o.pid",
            "--readiness-file", "/tmp/o.ready",
            "--registry-path", "/tmp/registry.json",
        ])
        .unwrap();
        assert_eq!(cli.idle_timeout, 1800);
        assert_eq!(cli.reranker_idle_secs, 600);
        assert!(cli.registry_path.is_some());
    }
}
