//! Route `ohara index --incremental` through a running `ohara serve` daemon.
//!
//! The daemon is an index writer (plan-33), so when one is already running
//! for this binary's version the incremental pass asks it to cover HEAD
//! instead of indexing in-process: one writer per store, and the daemon's
//! warm embedder is reused. Indexing only *finds* a daemon; it never spawns
//! one.

use super::{noop_report, up_to_date, Args};
use crate::commands::provider::ProviderArg;
use anyhow::{anyhow, Result};
use ohara_core::IndexerReport;
use ohara_engine::client::{registry_path, try_daemon_write, DaemonRoute};
use ohara_engine::ipc::{ErrorCode, Request, RequestMethod};
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
pub(crate) fn alive_same_version_socket() -> Option<PathBuf> {
    let reg = Registry::open(registry_path().ok()?).ok()?;
    reg.list_alive()
        .ok()?
        .into_iter()
        .find(|d| d.ohara_version == env!("CARGO_PKG_VERSION"))
        .map(|d| d.socket_path)
}

/// Run the incremental pass on a live daemon when [`choose_route`] says so.
/// `Ok(None)` means the caller indexes in-process; `Err` means the daemon
/// refused or may still be working, so nothing may run in-process.
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
    if out.indexed {
        super::notify::notify_daemons_of_invalidation(&id.worktree_path, Some(&socket)).await;
    }
    let mut report = noop_report();
    report.new_commits = usize::try_from(out.new_commits).unwrap_or(usize::MAX);
    Ok(Some(report))
}

/// Send `EnsureIndexed { sha: head }` for `worktree`, routed by the shared
/// write policy ([`DaemonCallResult::route_write`]).
///
/// `Ok(None)` means "index in-process" and is returned only when no daemon
/// work can be in flight: the socket is unreachable, or the daemon answered
/// `NotIndexed` (the first full index, which the daemon never does itself).
///
/// Once the request was sent, a timeout or read/decode failure is an error:
/// the daemon may still be indexing under its repo lock, and an in-process
/// run would be a second writer. A `NeedsRebuild` reply is also an error, so
/// the CLI never extends an index the daemon's live embedder refused.
///
/// [`DaemonCallResult::route_write`]: ohara_engine::client::DaemonCallResult::route_write
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
    let result = try_daemon_write(socket, req, ENSURE_INDEXED_TIMEOUT).await;
    match result.route_write(&[ErrorCode::NotIndexed]) {
        DaemonRoute::Served(out) => Ok(Some(out)),
        DaemonRoute::Fallback => Ok(None),
        DaemonRoute::Refuse(msg) => Err(anyhow!("index via daemon: {msg}")),
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

    /// Serve a fresh socket: read a request frame, then
    /// write `reply` (a JSON body) back. Returns the socket path.
    fn fake_daemon(dir: &Path, reply: serde_json::Value) -> PathBuf {
        let sock = dir.join("fake.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            // The reachability probe connects and hangs up first; only a
            // connection that sends a request frame gets the reply.
            while let Ok((mut conn, _)) = listener.accept().await {
                if ohara_engine::ipc::read_frame(&mut conn).await.is_err() {
                    continue;
                }
                let body = serde_json::to_vec(&reply).unwrap();
                let _ = ohara_engine::ipc::write_frame(&mut conn, &body).await;
            }
        });
        sock
    }

    fn error_reply(code: &str, message: &str) -> serde_json::Value {
        serde_json::json!({"id": 1, "error": {"code": code, "message": message}})
    }

    const SHA: &str = "0123456789012345678901234567890123456789";

    #[tokio::test]
    async fn unreachable_socket_falls_back_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let out = index_via_daemon(&dir.path().join("absent.sock"), dir.path(), SHA).await;
        assert!(matches!(out, Ok(None)), "got {out:?}");
    }

    #[tokio::test]
    async fn needs_rebuild_reply_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_daemon(
            dir.path(),
            error_reply("needs_rebuild", "index needs rebuild: x"),
        );
        let err = index_via_daemon(&sock, dir.path(), SHA).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("index needs rebuild: x"), "{msg}");
        assert!(msg.contains(ohara_engine::client::REBUILD_REMEDY), "{msg}");
    }

    #[tokio::test]
    async fn no_index_reply_falls_back_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_daemon(
            dir.path(),
            error_reply("not_indexed", "repo not indexed: x"),
        );
        let out = index_via_daemon(&sock, dir.path(), SHA).await;
        assert!(matches!(out, Ok(None)), "got {out:?}");
    }

    #[tokio::test]
    async fn other_error_reply_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_daemon(dir.path(), error_reply("internal", "boom"));
        let err = index_via_daemon(&sock, dir.path(), SHA).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn connection_dropped_after_send_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("drop.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            // Accept (probe, then request) and drop without replying.
            while let Ok((conn, _)) = listener.accept().await {
                drop(conn);
            }
        });
        let err = index_via_daemon(&sock, dir.path(), SHA).await.unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
    }
}
