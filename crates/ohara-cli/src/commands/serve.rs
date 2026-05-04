//! `ohara serve` — run the retrieval engine as a long-lived Unix-socket daemon.
//!
//! The process binds a Unix socket, writes a PID file and a readiness file,
//! then services IPC requests until either a `Shutdown` request arrives or the
//! optional idle-timeout watchdog fires.

use anyhow::{Context, Result};
use clap::Args;
use ohara_engine::{serve_unix, RetrievalEngine};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Path for the Unix domain socket.
    #[arg(long)]
    pub socket: PathBuf,
    /// Path to write the daemon PID into after the socket is ready.
    #[arg(long)]
    pub pid_file: PathBuf,
    /// Path to write "ready" into after the socket is bound and the PID file
    /// has been written. Callers can poll this file to know the daemon is up.
    #[arg(long)]
    pub readiness_file: PathBuf,
    /// Exit after this many seconds with no incoming requests.
    /// Set to 0 to disable the idle-exit watchdog (debug only).
    #[arg(long, default_value_t = 1800)]
    pub idle_timeout: u64,
    /// Path to the daemon registry JSON file. When provided, the daemon
    /// updates its `last_health_unix` timestamp every 30 seconds so the
    /// registry stays current.
    #[arg(long)]
    pub registry_path: Option<PathBuf>,
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new)
            .await
            .context("spawn_blocking FastEmbedProvider")?
            .context("FastEmbedProvider::new")?,
    );
    let reranker: Arc<dyn ohara_core::embed::RerankProvider> = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new)
            .await
            .context("spawn_blocking FastEmbedReranker")?
            .context("FastEmbedReranker::new")?,
    );
    let engine = Arc::new(RetrievalEngine::new(embedder, reranker));

    let stop = CancellationToken::new();
    let listener_engine = engine.clone();
    let listener_stop = stop.clone();
    let socket = args.socket.clone();
    let listener =
        tokio::spawn(async move { serve_unix(listener_engine, &socket, listener_stop).await });

    wait_for_socket(&args.socket, Duration::from_secs(10)).await?;

    std::fs::write(&args.pid_file, std::process::id().to_string()).context("write pid file")?;
    std::fs::write(&args.readiness_file, "ready").context("write readiness file")?;

    info!(
        socket = ?args.socket,
        pid_file = ?args.pid_file,
        readiness_file = ?args.readiness_file,
        "ohara serve ready"
    );

    if let Some(reg_path) = args.registry_path.clone() {
        let pid = std::process::id();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if let Ok(reg) = ohara_engine::registry::Registry::open(&reg_path) {
                    let _ = reg.touch_health(pid);
                }
            }
        });
    }

    if args.idle_timeout > 0 {
        let idle = Duration::from_secs(args.idle_timeout);
        let watchdog_engine = engine.clone();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle / 2).await;
                if watchdog_engine.idle_for() >= idle {
                    info!(?idle, "idle timeout reached, shutting down");
                    watchdog_stop.cancel();
                    break;
                }
            }
        });
    }

    let _ = listener
        .await
        .map_err(|e| anyhow::anyhow!("listener join: {e}"))?;

    let _ = std::fs::remove_file(&args.pid_file);
    let _ = std::fs::remove_file(&args.readiness_file);
    Ok(())
}

/// Poll until `p` exists or `total` elapses. Returns an error on timeout.
async fn wait_for_socket(p: &std::path::Path, total: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < total {
        if p.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("socket {p:?} did not appear within {total:?}")
}
