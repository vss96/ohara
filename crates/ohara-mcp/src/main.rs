use anyhow::Result;

mod server;
mod tools;

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
