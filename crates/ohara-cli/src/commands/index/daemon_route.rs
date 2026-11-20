//! Route `ohara index --incremental` through a running `ohara serve` daemon.
//!
//! The daemon is an index writer (plan-33), so when one is already running
//! for this binary's version the incremental pass asks it to cover HEAD
//! instead of indexing in-process: one writer per store, and the daemon's
//! warm embedder is reused. Indexing only *finds* a daemon; it never spawns
//! one.

use super::{noop_report, up_to_date, Args};
use crate::commands::provider::ProviderArg;
use anyhow::Result;
use ohara_core::IndexerReport;
use ohara_engine::client::{registry_path, Client};
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::registry::Registry;
use ohara_engine::EnsureIndexedOutcome;
use ohara_git::RepoIdentity;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An index pass can run for a long time; the daemon answers only when done.
const ENSURE_INDEXED_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What the incremental path should do.
pub(super) enum Route {
    /// Hand the work to the daemon listening on this socket.
    Daemon(PathBuf),
    /// Index in this process.
    InProcess,
}

/// Pure decision: `Daemon` only when every condition holds.
pub(super) fn choose_route(
    incremental: bool,
    force: bool,
    rebuild: bool,
    no_daemon: bool,
    explicit_tuning: bool,
    alive_same_version_socket: Option<PathBuf>,
) -> Route {
    if !incremental || force || rebuild || no_daemon || explicit_tuning {
        return Route::InProcess;
    }
    match alive_same_version_socket {
        Some(socket) => Route::Daemon(socket),
        None => Route::InProcess,
    }
}

/// True when the caller tuned the in-process indexer, which the daemon
/// cannot honour. `--embed-provider cpu` does not count: the post-commit
/// hook passes it and the daemon embedder is CPU anyway.
fn explicit_tuning(args: &Args) -> bool {
    let provider_tuned = matches!(args.embed_provider, Some(p) if p != ProviderArg::Cpu);
    provider_tuned
        || args.threads.is_some()
        || args.workers.is_some()
        || args.commit_batch.is_some()
        || args.embed_batch.is_some()
        || args.embed_cache != super::EmbedCacheArg::Off
}

/// Socket of a live daemon built from this binary's version, if any.
/// Registry trouble counts as "none": indexing then proceeds in-process.
fn alive_same_version_socket() -> Option<PathBuf> {
    let reg = Registry::open(registry_path().ok()?).ok()?;
    reg.list_alive()
        .ok()?
        .into_iter()
        .find(|d| d.ohara_version == env!("CARGO_PKG_VERSION"))
        .map(|d| d.socket_path)
}

/// Run the incremental pass on a live daemon when [`choose_route`] says so.
/// `Ok(None)` means the caller indexes in-process.
pub(super) async fn try_route(
    args: &Args,
    no_daemon: bool,
    id: &RepoIdentity,
) -> Result<Option<IndexerReport>> {
    let alive = match args.incremental && !no_daemon {
        true => alive_same_version_socket(),
        false => None,
    };
    let route = choose_route(
        args.incremental,
        args.force,
        args.rebuild,
        no_daemon,
        explicit_tuning(args),
        alive,
    );
    let Route::Daemon(socket) = route else {
        return Ok(None);
    };
    let head = up_to_date::head_sha(id)?;
    let Some(out) = index_via_daemon(&socket, &id.worktree_path, head.as_str()).await? else {
        return Ok(None);
    };
    println!(
        "indexed {} commit(s) via daemon in {} ms",
        out.new_commits, out.elapsed_ms
    );
    let mut report = noop_report();
    report.new_commits = usize::try_from(out.new_commits).unwrap_or(usize::MAX);
    Ok(Some(report))
}

/// Send `EnsureIndexed { sha: head }` for `worktree`. `Ok(None)` means
/// "fall back in-process": the daemon was unreachable, timed out, refused
/// (`NeedsRebuild`, no index, `NotImplemented`, ...) or answered garbage.
/// The in-process path then runs and reports the problem as it always has.
pub(super) async fn index_via_daemon(
    socket: &Path,
    worktree: &Path,
    head: &str,
) -> Result<Option<EnsureIndexedOutcome>> {
    let req = Request {
        id: 1,
        repo_path: Some(worktree.to_string_lossy().into_owned()),
        method: RequestMethod::EnsureIndexed { sha: head.into() },
    };
    let resp = match Client::connect(socket)
        .call_with_timeout(req, ENSURE_INDEXED_TIMEOUT)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "daemon unreachable; indexing in-process");
            return Ok(None);
        }
    };
    if let Some(err) = resp.error {
        tracing::warn!(code = ?err.code, message = %err.message, "daemon declined to index; indexing in-process");
        return Ok(None);
    }
    let Some(value) = resp.result else {
        tracing::warn!("daemon response had no result; indexing in-process");
        return Ok(None);
    };
    match serde_json::from_value(value) {
        Ok(out) => Ok(Some(out)),
        Err(e) => {
            tracing::warn!(error = %e, "undecodable daemon response; indexing in-process");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn routes_to_daemon_only_for_plain_incremental_with_a_live_daemon() {
        let sock = Some(PathBuf::from("/tmp/s"));
        assert!(matches!(
            choose_route(true, false, false, false, false, sock.clone()),
            Route::Daemon(_)
        ));
        // full index
        assert!(matches!(
            choose_route(false, false, false, false, false, sock.clone()),
            Route::InProcess
        ));
        // --force
        assert!(matches!(
            choose_route(true, true, false, false, false, sock.clone()),
            Route::InProcess
        ));
        // --rebuild
        assert!(matches!(
            choose_route(true, false, true, false, false, sock.clone()),
            Route::InProcess
        ));
        // --no-daemon
        assert!(matches!(
            choose_route(true, false, false, true, false, sock.clone()),
            Route::InProcess
        ));
        // tuned
        assert!(matches!(
            choose_route(true, false, false, false, true, sock),
            Route::InProcess
        ));
        // none alive
        assert!(matches!(
            choose_route(true, false, false, false, false, None),
            Route::InProcess
        ));
    }
}
