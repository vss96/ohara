//! `ohara prune [PATH]`: remove index rows for commits that no branch,
//! tag or worktree `HEAD` reaches any more, and tidy the indexed tips
//! (plan-33 follow-up F4).
//!
//! Like `ohara index --incremental`, it hands the work to a running
//! daemon of this binary's version when there is one (one writer per
//! store), and runs in-process only when no daemon is reachable. The
//! in-process path needs only the index and git, so no model loads.

use crate::commands::index::daemon_route::alive_same_version_socket;
use crate::commands::index::notify::notify_daemons_of_invalidation;
use anyhow::{anyhow, bail, Result};
use clap::Args as ClapArgs;
use ohara_engine::client::Client;
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::PruneOutcome;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Prune walks every ref; the daemon answers only when done.
const PRUNE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo or any of its worktrees (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run `ohara prune`. `no_daemon` forces the in-process path.
pub async fn run(args: Args, no_daemon: bool) -> Result<PruneOutcome> {
    let id = super::resolve_repo_id(&args.path)?;
    let socket = match no_daemon {
        true => None,
        false => alive_same_version_socket(),
    };
    let routed = match socket.as_deref() {
        Some(s) => prune_via_daemon(s, &id.worktree_path).await?,
        None => None,
    };
    // The routing daemon already refreshed its own caches; skip it.
    let (out, skip) = match routed {
        Some(out) => (out, socket.as_deref()),
        None => (
            ohara_engine::prune_unreachable_in_process(&id.worktree_path).await?,
            None,
        ),
    };
    if out.commits_deleted > 0 || out.tips_removed > 0 {
        notify_daemons_of_invalidation(&id.worktree_path, skip).await;
    }
    println!(
        "removed {} unreachable commit(s) and {} tip(s)",
        out.commits_deleted, out.tips_removed
    );
    Ok(out)
}

/// Send `PruneUnreachable` for `worktree` to the daemon on `socket`.
///
/// `Ok(None)` means "prune in-process" and is returned only when the
/// socket is unreachable, so no daemon work can be in flight. Once the
/// request was sent, any failure is an error: the daemon may still be
/// pruning under its repo lock.
async fn prune_via_daemon(socket: &Path, worktree: &Path) -> Result<Option<PruneOutcome>> {
    let client = Client::connect(socket);
    if !client.reachable().await {
        tracing::debug!(socket = %socket.display(), "daemon unreachable; pruning in-process");
        return Ok(None);
    }
    let req = Request {
        id: 1,
        repo_path: Some(worktree.to_string_lossy().into_owned()),
        method: RequestMethod::PruneUnreachable,
    };
    let resp = client
        .call_with_timeout(req, PRUNE_TIMEOUT)
        .await
        .map_err(|e| anyhow!("daemon did not finish ({e}); it may still be pruning"))?;
    if let Some(err) = resp.error {
        bail!("daemon failed to prune: {}", err.message);
    }
    let value = resp
        .result
        .ok_or_else(|| anyhow!("daemon response had no result; it may still be pruning"))?;
    let out = serde_json::from_value(value)
        .map_err(|e| anyhow!("undecodable daemon response: {e}; it may still be pruning"))?;
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_socket_falls_back_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let out = prune_via_daemon(&dir.path().join("none.sock"), dir.path())
            .await
            .unwrap();
        assert!(out.is_none());
    }
}
