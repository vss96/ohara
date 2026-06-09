//! Long-lived daemon runner shared by `ohara serve` and `ohara-mcp serve`.
//!
//! Owns the socket listener plus the watchdogs (readiness, registry
//! heartbeat, whole-process idle exit, reranker idle unload). Binaries
//! construct the engine (or call [`run_daemon`] for the default CPU
//! providers) — keeping provider choice in one place per binary.

use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::server::serve_unix;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct DaemonOptions {
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub readiness_file: PathBuf,
    /// Exit after this many seconds with no requests. 0 disables.
    pub idle_timeout_secs: u64,
    pub registry_path: Option<PathBuf>,
    /// Unload the lazily-loaded reranker session after this many seconds
    /// without a rerank. 0 disables (plan-29 tiered unload).
    pub reranker_idle_secs: u64,
}

/// Construct the default engine (CPU embedder, lazy reranker) and run.
pub async fn run_daemon(opts: DaemonOptions) -> crate::Result<()> {
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> = Arc::new(
        tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new)
            .await
            .map_err(|e| EngineError::Internal(format!("spawn_blocking embedder: {e}")))?
            .map_err(|e| EngineError::Embed(e.to_string()))?,
    );
    let reranker: Arc<dyn ohara_core::embed::RerankProvider> =
        Arc::new(ohara_embed::LazyFastEmbedReranker::new());
    let engine = Arc::new(RetrievalEngine::new(embedder, reranker));
    run_daemon_with_engine(engine, opts).await
}

/// Bind, write pid/readiness files, run watchdogs, serve until Shutdown
/// or idle exit. Testable with any engine.
pub async fn run_daemon_with_engine(
    engine: Arc<RetrievalEngine>,
    opts: DaemonOptions,
) -> crate::Result<()> {
    let stop = CancellationToken::new();
    let listener_engine = engine.clone();
    let listener_stop = stop.clone();
    let socket = opts.socket.clone();
    let mut listener =
        tokio::spawn(async move { serve_unix(listener_engine, &socket, listener_stop).await });

    // Surface a bind/startup error immediately rather than timing out.
    let ready = wait_for_socket(&opts.socket, Duration::from_secs(10));
    tokio::select! {
        biased;
        res = &mut listener => {
            return match res {
                Ok(Ok(())) => Err(EngineError::Internal("listener exited before socket was ready".into())),
                Ok(Err(e)) => Err(EngineError::Internal(format!("serve_unix failed at startup: {e}"))),
                Err(e) => Err(EngineError::Internal(format!("listener task join: {e}"))),
            }
        }
        res = ready => res?,
    }

    std::fs::write(&opts.pid_file, std::process::id().to_string())
        .map_err(|e| EngineError::Internal(format!("write pid file: {e}")))?;
    std::fs::write(&opts.readiness_file, "ready")
        .map_err(|e| EngineError::Internal(format!("write readiness file: {e}")))?;
    info!(socket = ?opts.socket, pid_file = ?opts.pid_file, "ohara daemon ready");

    if let Some(reg_path) = &opts.registry_path {
        if let Some(daemon_root) = reg_path.parent().and_then(|p| p.parent()) {
            let root = daemon_root.to_path_buf();
            tokio::spawn(async move {
                sweep_stale_versions(&root, env!("CARGO_PKG_VERSION")).await;
            });
        }
    }

    if let Some(reg_path) = opts.registry_path.clone() {
        let pid = std::process::id();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if let Ok(reg) = crate::registry::Registry::open(&reg_path) {
                    let _ = reg.touch_health(pid);
                }
            }
        });
    }

    if opts.idle_timeout_secs > 0 {
        let idle = Duration::from_secs(opts.idle_timeout_secs);
        let watchdog_engine = engine.clone();
        let watchdog_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle / 2).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if watchdog_engine.idle_for() >= idle {
                    info!(?idle, "idle timeout reached, shutting down");
                    watchdog_stop.cancel();
                    break;
                }
            }
        });
    }

    if opts.reranker_idle_secs > 0 {
        let idle = Duration::from_secs(opts.reranker_idle_secs);
        let watchdog_engine = engine.clone();
        let watchdog_stop = stop.clone();
        // Check at most once a minute; for small thresholds, at the
        // threshold itself (keeps behavior predictable).
        let period = Duration::from_secs(opts.reranker_idle_secs.clamp(1, 60));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                if watchdog_stop.is_cancelled() {
                    break;
                }
                if watchdog_engine.unload_idle_reranker(idle).await {
                    info!(
                        idle_secs = idle.as_secs(),
                        "reranker session unloaded after idle"
                    );
                }
            }
        });
    }

    let listener_result = listener
        .await
        .map_err(|e| EngineError::Internal(format!("listener join: {e}")))?;
    let _ = std::fs::remove_file(&opts.pid_file);
    let _ = std::fs::remove_file(&opts.readiness_file);
    listener_result
}

async fn wait_for_socket(p: &std::path::Path, total: Duration) -> crate::Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < total {
        if p.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(EngineError::Internal(format!(
        "socket {p:?} did not appear within {total:?}"
    )))
}

/// Best-effort cleanup of daemons left behind by other ohara versions.
///
/// `daemon_root` is the parent of the per-version registry dirs
/// (`<cache>/ohara/daemon`). For every sibling version dir: shut down
/// its live daemons over their sockets, then remove the dir once empty.
/// Failures are logged and skipped — the old daemons' own idle timeout
/// remains the backstop.
pub(crate) async fn sweep_stale_versions(daemon_root: &std::path::Path, current_version: &str) {
    let entries = match std::fs::read_dir(daemon_root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == current_version {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        let reg = match crate::registry::Registry::open(entry.path().join("registry.json")) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(dir = ?entry.path(), error = %e, "sweep: registry open failed");
                continue;
            }
        };
        let alive = match reg.list_alive() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let mut all_stopped = true;
        for d in alive {
            let req = crate::ipc::Request {
                id: 1,
                repo_path: None,
                method: crate::ipc::RequestMethod::Shutdown,
            };
            match crate::client::Client::connect(&d.socket_path)
                .call(req)
                .await
            {
                Ok(_) => {
                    let _ = reg.unregister(d.pid);
                    tracing::info!(pid = d.pid, "sweep: shut down stale-version daemon");
                }
                Err(e) => {
                    all_stopped = false;
                    tracing::warn!(pid = d.pid, error = %e, "sweep: shutdown failed");
                }
            }
        }
        if all_stopped {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ipc::{Request, RequestMethod};
    use std::sync::Arc;

    use crate::engine::tests::make_test_engine;

    #[tokio::test]
    async fn sweep_removes_stale_version_dirs_with_dead_daemons() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path(); // plays the role of `<cache>/ohara/daemon`
        let stale = root.join("0.0.1");
        let current = root.join(env!("CARGO_PKG_VERSION"));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        // Dead-pid record in the stale-version registry.
        let reg = crate::registry::Registry::open(stale.join("registry.json")).unwrap();
        reg.register(crate::registry::DaemonRecord {
            pid: u32::MAX - 1, // not a live pid
            socket_path: stale.join("dead.sock"),
            ohara_version: "0.0.1".into(),
            ohara_git_sha: None,
            started_at_unix: 1,
            last_health_unix: 1,
        })
        .unwrap();

        sweep_stale_versions(root, env!("CARGO_PKG_VERSION")).await;

        assert!(!stale.exists(), "stale version dir must be removed");
        assert!(current.exists(), "current version dir must be untouched");
    }

    #[tokio::test]
    async fn run_daemon_with_engine_serves_ping_until_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = DaemonOptions {
            socket: tmp.path().join("d.sock"),
            pid_file: tmp.path().join("d.pid"),
            readiness_file: tmp.path().join("d.ready"),
            idle_timeout_secs: 0, // watchdog off for the test
            registry_path: None,
            reranker_idle_secs: 0,
        };
        let engine = Arc::new(make_test_engine());
        let socket = opts.socket.clone();
        let ready = opts.readiness_file.clone();
        let task = tokio::spawn(async move { run_daemon_with_engine(engine, opts).await });

        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ready.exists(), "daemon did not become ready");

        let ping = crate::client::Client::connect(&socket)
            .call(Request {
                id: 1,
                repo_path: None,
                method: RequestMethod::Ping,
            })
            .await
            .expect("ping");
        assert!(ping.error.is_none());

        let _ = crate::client::Client::connect(&socket)
            .call(Request {
                id: 2,
                repo_path: None,
                method: RequestMethod::Shutdown,
            })
            .await
            .expect("shutdown");
        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("daemon must exit after Shutdown")
            .expect("join");
        assert!(joined.is_ok(), "daemon exited with error: {joined:?}");
    }
}
