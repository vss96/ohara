//! `ohara daemon` subcommands: inspect and manage the daemon registry.

use anyhow::Result;
use clap::Subcommand;
use ohara_engine::client::{registry_path, Client};
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::registry::Registry;
use tracing;

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    /// List all registered daemon records (including stale/dead entries).
    List,
    /// Show currently-alive, healthy daemons and their idle time.
    Status,
    /// Send a Shutdown request to every alive daemon and remove their entries.
    Stop,
}

pub async fn run(action: DaemonAction) -> Result<()> {
    let reg_path = registry_path().map_err(|e| anyhow::anyhow!("registry_path: {e}"))?;
    let reg = Registry::open(&reg_path).map_err(|e| anyhow::anyhow!("Registry::open: {e}"))?;

    match action {
        DaemonAction::List => {
            let all = reg.list().map_err(|e| anyhow::anyhow!("list: {e}"))?;
            println!("PID\tVERSION\tSTARTED\tHEALTH\tBUSY\tSOCKET");
            for d in all {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    d.pid,
                    d.ohara_version,
                    d.started_at_unix,
                    d.last_health_unix,
                    d.busy,
                    d.socket_path.display()
                );
            }
        }
        DaemonAction::Status => {
            let alive = reg
                .list_alive()
                .map_err(|e| anyhow::anyhow!("list_alive: {e}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            for d in alive {
                let last_heartbeat = now.saturating_sub(d.last_health_unix);
                println!(
                    "{}\t{}\tstarted_at={}\tlast_heartbeat={}s",
                    d.pid, d.ohara_version, d.started_at_unix, last_heartbeat
                );
            }
        }
        DaemonAction::Stop => {
            let alive = reg
                .list_alive()
                .map_err(|e| anyhow::anyhow!("list_alive: {e}"))?;
            for d in alive {
                let req = Request {
                    id: 1,
                    repo_path: None,
                    method: RequestMethod::Shutdown,
                };
                match Client::connect(&d.socket_path).call(req).await {
                    Ok(_) => {
                        let _ = reg.unregister(d.pid);
                    }
                    Err(e) => {
                        tracing::warn!(
                            pid = d.pid,
                            error = %e,
                            "daemon shutdown call failed; leaving registry entry for stale-prune"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
