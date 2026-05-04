//! Unix-socket listener + per-request dispatch for the `ohara serve` daemon.

use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::engine::RetrievalEngine;
use crate::error::EngineError;
use crate::ipc::{read_frame, write_frame};
use crate::ipc::{ErrorCode, ErrorPayload, Request, RequestMethod, Response};

/// Bind a Unix socket at `socket_path`, accept connections until `stop` is
/// cancelled, and dispatch one request per connection.
///
/// Each accepted connection is handled in its own `tokio::spawn` task.
/// After a [`RequestMethod::Shutdown`] is dispatched the stop token is
/// cancelled, which unblocks the `select!` and exits the loop cleanly.
pub async fn serve_unix(
    engine: Arc<RetrievalEngine>,
    socket_path: &Path,
    stop: CancellationToken,
) -> crate::Result<()> {
    if socket_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            let meta = std::fs::symlink_metadata(socket_path)
                .map_err(|e| EngineError::Internal(format!("stat {socket_path:?}: {e}")))?;
            if !meta.file_type().is_socket() {
                return Err(EngineError::Internal(format!(
                    "refusing to unlink non-socket path {socket_path:?}"
                )));
            }
        }
        std::fs::remove_file(socket_path)
            .map_err(|e| EngineError::Internal(format!("remove stale socket: {e}")))?;
    }
    let listener = UnixListener::bind(socket_path)
        .map_err(|e| EngineError::Internal(format!("bind {socket_path:?}: {e}")))?;
    set_socket_perms(socket_path)?;
    info!(socket=?socket_path, "ohara serve listening");
    let stop_for_dispatch = stop.clone();
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            res = listener.accept() => {
                match res {
                    Ok((conn, _addr)) => {
                        let eng = engine.clone();
                        let stop_d = stop_for_dispatch.clone();
                        handlers.spawn(async move {
                            if let Err(e) = handle_connection(eng, conn, stop_d).await {
                                warn!("connection handler: {e}");
                            }
                        });
                    }
                    Err(e) => error!("accept error: {e}"),
                }
            }
        }
    }
    // Drain in-flight handlers with a bounded 5-second grace period so
    // requests that are mid-flight are not abruptly aborted.
    let drain = async { while handlers.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drain).await;
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

#[cfg(unix)]
fn set_socket_perms(path: &Path) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| EngineError::Internal(format!("chmod 0600: {e}")))
}

async fn handle_connection(
    engine: Arc<RetrievalEngine>,
    mut conn: UnixStream,
    stop: CancellationToken,
) -> crate::Result<()> {
    let body = read_frame(&mut conn).await?;
    let req: Request = serde_json::from_slice(&body)
        .map_err(|e| EngineError::Internal(format!("decode request: {e}")))?;
    engine.touch();
    let is_shutdown = matches!(req.method, RequestMethod::Shutdown);
    let resp = dispatch(&engine, req).await;
    let bytes = serde_json::to_vec(&resp)
        .map_err(|e| EngineError::Internal(format!("encode response: {e}")))?;
    write_frame(&mut conn, &bytes).await?;
    if is_shutdown {
        // Reply has been flushed by write_frame; now signal the loop to stop.
        stop.cancel();
    }
    Ok(())
}

async fn dispatch(engine: &RetrievalEngine, req: Request) -> Response {
    let id = req.id;
    let result: crate::Result<serde_json::Value> = match req.method {
        RequestMethod::Ping => Ok(serde_json::json!({"pong": true})),
        RequestMethod::Shutdown => Ok(serde_json::json!({"shutting_down": true})),
        RequestMethod::FindPattern(q) => {
            let path = match req.repo_path {
                Some(p) => p,
                None => {
                    return error_response(
                        id,
                        ErrorCode::Internal,
                        "find_pattern requires repo_path",
                    )
                }
            };
            match engine.find_pattern(&path, q).await {
                Ok(r) => serde_json::to_value(&r).map_err(|e| EngineError::Internal(e.to_string())),
                Err(e) => Err(e),
            }
        }
        RequestMethod::ExplainChange(q) => {
            let path = match req.repo_path {
                Some(p) => p,
                None => {
                    return error_response(
                        id,
                        ErrorCode::Internal,
                        "explain_change requires repo_path",
                    )
                }
            };
            match engine.explain_change(&path, q).await {
                Ok(r) => serde_json::to_value(&r).map_err(|e| EngineError::Internal(e.to_string())),
                Err(e) => Err(e),
            }
        }
        RequestMethod::InvalidateRepo => {
            let path = match req.repo_path {
                Some(p) => p,
                None => {
                    return error_response(
                        id,
                        ErrorCode::Internal,
                        "invalidate_repo requires repo_path",
                    )
                }
            };
            match engine.invalidate_repo(&path).await {
                Ok(()) => Ok(serde_json::json!({"invalidated": true})),
                Err(e) => Err(e),
            }
        }
        // Not yet implemented — callers should fall back to in-process logic.
        RequestMethod::IndexStatus => Err(EngineError::NotImplemented {
            method: "index_status",
        }),
        RequestMethod::Metrics => Err(EngineError::NotImplemented {
            method: "metrics",
        }),
    };
    match result {
        Ok(v) => Response {
            id,
            result: Some(v),
            error: None,
        },
        Err(e) => Response {
            id,
            result: None,
            error: Some(engine_error_to_payload(e)),
        },
    }
}

fn error_response(id: u64, code: ErrorCode, msg: &str) -> Response {
    Response {
        id,
        result: None,
        error: Some(ErrorPayload {
            code,
            message: msg.to_string(),
        }),
    }
}

fn engine_error_to_payload(e: EngineError) -> ErrorPayload {
    let (code, message) = match &e {
        EngineError::NoIndex { .. } => (ErrorCode::NotIndexed, e.to_string()),
        EngineError::NeedsRebuild { .. } => (ErrorCode::NeedsRebuild, e.to_string()),
        EngineError::NotImplemented { .. } => (ErrorCode::NotImplemented, e.to_string()),
        _ => (ErrorCode::Internal, e.to_string()),
    };
    ErrorPayload { code, message }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::tests::make_test_engine;
    use crate::ipc::{Request, RequestMethod, Response};

    #[tokio::test]
    async fn server_responds_to_ping() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ohara.sock");
        let engine = Arc::new(make_test_engine());
        let stop = tokio_util::sync::CancellationToken::new();
        let task = {
            let s = sock.clone();
            let stop2 = stop.clone();
            tokio::spawn(async move { serve_unix(engine, &s, stop2).await })
        };
        // Wait for the socket file to appear (up to 500 ms).
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut conn = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = Request {
            id: 1,
            repo_path: None,
            method: RequestMethod::Ping,
        };
        let body = serde_json::to_vec(&req).unwrap();
        crate::ipc::write_frame(&mut conn, &body).await.unwrap();
        let resp_body = crate::ipc::read_frame(&mut conn).await.unwrap();
        let resp: Response = serde_json::from_slice(&resp_body).unwrap();
        // result must be present and error must be absent for a ping.
        assert!(
            resp.result.is_some(),
            "ping should return a result: {resp:?}"
        );
        assert!(
            resp.error.is_none(),
            "ping must not return an error: {resp:?}"
        );
        stop.cancel();
        let _ = task.await;
    }

    #[tokio::test]
    async fn server_shutdown_stops_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ohara2.sock");
        let engine = Arc::new(make_test_engine());
        let stop = tokio_util::sync::CancellationToken::new();
        let task = {
            let s = sock.clone();
            let stop2 = stop.clone();
            tokio::spawn(async move { serve_unix(engine, &s, stop2).await })
        };
        // Wait for the socket file to appear (up to 500 ms).
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut conn = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = Request {
            id: 2,
            repo_path: None,
            method: RequestMethod::Shutdown,
        };
        let body = serde_json::to_vec(&req).unwrap();
        crate::ipc::write_frame(&mut conn, &body).await.unwrap();
        // Read the ack so the handler has flushed before the cancel propagates.
        let resp_body = crate::ipc::read_frame(&mut conn).await.unwrap();
        let resp: Response = serde_json::from_slice(&resp_body).unwrap();
        assert!(
            resp.result.is_some(),
            "shutdown ack must carry a result: {resp:?}"
        );
        // The listener task must terminate within 1 s of the Shutdown ack.
        let timeout = tokio::time::timeout(std::time::Duration::from_secs(1), task);
        let join_result = timeout
            .await
            .expect("server must stop within 1 s after Shutdown");
        join_result
            .expect("task must not panic")
            .expect("serve_unix must return Ok");
    }

    /// Shared helper: spin up a listener, send `req`, return the response,
    /// then cancel the listener.
    async fn round_trip(req: Request) -> Response {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("rt.sock");
        let engine = Arc::new(make_test_engine());
        let stop = tokio_util::sync::CancellationToken::new();
        let task = {
            let s = sock.clone();
            let stop2 = stop.clone();
            tokio::spawn(async move { serve_unix(engine, &s, stop2).await })
        };
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut conn = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let body = serde_json::to_vec(&req).unwrap();
        crate::ipc::write_frame(&mut conn, &body).await.unwrap();
        let resp_body = crate::ipc::read_frame(&mut conn).await.unwrap();
        let resp: Response = serde_json::from_slice(&resp_body).unwrap();
        stop.cancel();
        let _ = task.await;
        resp
    }

    #[tokio::test]
    async fn index_status_returns_not_implemented_error() {
        let req = Request {
            id: 10,
            repo_path: None,
            method: RequestMethod::IndexStatus,
        };
        let resp = round_trip(req).await;
        assert!(
            resp.result.is_none(),
            "IndexStatus must not return a result: {resp:?}"
        );
        let err = resp.error.expect("IndexStatus must return an error");
        assert_eq!(
            err.code,
            ErrorCode::NotImplemented,
            "code must be not_implemented, got {:?}",
            err.code
        );
    }

    #[tokio::test]
    async fn metrics_returns_not_implemented_error() {
        let req = Request {
            id: 11,
            repo_path: None,
            method: RequestMethod::Metrics,
        };
        let resp = round_trip(req).await;
        assert!(
            resp.result.is_none(),
            "Metrics must not return a result: {resp:?}"
        );
        let err = resp.error.expect("Metrics must return an error");
        assert_eq!(
            err.code,
            ErrorCode::NotImplemented,
            "code must be not_implemented, got {:?}",
            err.code
        );
    }
}
