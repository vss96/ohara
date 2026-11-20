//! `try_daemon_call`: wraps discover + Client::call and reports a typed
//! [`DaemonCallResult`] so callers can tell "no daemon" from "daemon
//! unreachable" from "daemon answered with an error".

use crate::client::outcome::DaemonCallResult;
use crate::client::{Client, DaemonHandle};
use crate::ipc::Request;

/// Try to route `req` through a running daemon.
///
/// 1. Calls `discover()` to obtain a [`DaemonHandle`].
/// 2. Connects to the daemon's socket and issues `req`.
///
/// Never propagates an error: every failure is a [`DaemonCallResult`]
/// variant. Use [`DaemonCallResult::route`] to decide what to do.
pub async fn try_daemon_call(
    discover: impl FnOnce() -> crate::Result<Option<DaemonHandle>>,
    req: Request,
) -> DaemonCallResult {
    let h = match discover() {
        Ok(Some(h)) => h,
        Ok(None) => {
            tracing::debug!("no compatible daemon; falling back to standalone");
            return DaemonCallResult::Unavailable;
        }
        Err(e) => {
            tracing::warn!(error = %e, "daemon discover failed; falling back to standalone");
            return DaemonCallResult::ConnectFailed(format!("discover: {e}"));
        }
    };
    let resp = match Client::connect(&h.socket_path).call(req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error=%e, pid=h.pid, "daemon call failed; falling back to standalone");
            return DaemonCallResult::ConnectFailed(format!("call: {e}"));
        }
    };
    if let Some(err) = resp.error {
        return DaemonCallResult::DaemonErrored(err);
    }
    match resp.result {
        Some(value) => DaemonCallResult::Ok(value),
        None => DaemonCallResult::ConnectFailed("daemon response missing result".into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod try_call_tests {
    use super::*;
    use crate::ipc::{Request, RequestMethod};

    #[tokio::test]
    async fn discover_error_is_connect_failed() {
        let resp = try_daemon_call(
            || Err(crate::error::EngineError::Internal("nope".into())),
            Request {
                id: 1,
                repo_path: None,
                method: RequestMethod::Ping,
            },
        )
        .await;
        assert!(
            matches!(resp, DaemonCallResult::ConnectFailed(_)),
            "discover error must be ConnectFailed"
        );
    }

    #[tokio::test]
    async fn dead_socket_is_connect_failed() {
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
        assert!(
            matches!(resp, DaemonCallResult::ConnectFailed(_)),
            "connect to dead socket must be ConnectFailed"
        );
    }
}
