//! `try_daemon_call`: wraps discover + Client::call with transparent fallback.
//!
//! Returns `None` on any failure so the caller can fall back to standalone mode.

use crate::client::{Client, DaemonHandle};
use crate::ipc::{Request, Response};

/// Try to route `req` through a running daemon.
///
/// 1. Calls `discover()` to obtain a [`DaemonHandle`].
/// 2. Connects to the daemon's socket and issues `req`.
/// 3. Returns `Some(response)` on success.
///
/// Returns `None` — without propagating the error — whenever anything fails,
/// so the caller can transparently fall back to standalone execution.
pub async fn try_daemon_call(
    discover: impl FnOnce() -> crate::Result<Option<DaemonHandle>>,
    req: Request,
) -> Option<Response> {
    let h = match discover() {
        Ok(Some(h)) => h,
        _ => return None,
    };
    match Client::connect(&h.socket_path).call(req).await {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(error=%e, pid=h.pid, "daemon call failed; falling back to standalone");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod try_call_tests {
    use super::*;
    use crate::ipc::{Request, RequestMethod};

    #[tokio::test]
    async fn discover_error_returns_none() {
        let resp = try_daemon_call(
            || Err(crate::error::EngineError::Internal("nope".into())),
            Request {
                id: 1,
                repo_path: None,
                method: RequestMethod::Ping,
            },
        )
        .await;
        assert!(resp.is_none(), "discover error must propagate as None");
    }

    #[tokio::test]
    async fn dead_socket_returns_none() {
        let dead = std::path::PathBuf::from("/tmp/ohara-dead-socket-from-test.sock");
        let _ = std::fs::remove_file(&dead); // belt-and-suspenders
        let h = DaemonHandle {
            socket_path: dead,
            pid: 0,
            spawned: false,
        };
        let resp = try_daemon_call(
            move || Ok(Some(h)),
            Request {
                id: 1,
                repo_path: None,
                method: RequestMethod::Ping,
            },
        )
        .await;
        assert!(resp.is_none(), "connect to dead socket must yield None");
    }
}
