//! `ohara serve` — run the retrieval engine as a long-lived Unix-socket daemon.
//!
//! Argument parsing lives here; the runner itself lives in
//! [`ohara_engine::daemon`] so both `ohara-cli` and `ohara-mcp` can share it.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

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
    /// Drop the lazily-loaded reranker session after this many seconds
    /// without a rerank. 0 disables idle unload.
    #[arg(long, default_value_t = 600)]
    pub reranker_idle_secs: u64,
}

pub async fn run(args: ServeArgs) -> Result<()> {
    ohara_engine::daemon::run_daemon(ohara_engine::daemon::DaemonOptions {
        socket: args.socket,
        pid_file: args.pid_file,
        readiness_file: args.readiness_file,
        idle_timeout_secs: args.idle_timeout,
        registry_path: args.registry_path,
        reranker_idle_secs: args.reranker_idle_secs,
    })
    .await
    .map_err(|e| anyhow::anyhow!("daemon: {e}"))
}
