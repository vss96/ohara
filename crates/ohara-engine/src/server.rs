//! Unix-socket listener + per-request dispatch for the `ohara serve` daemon.

use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::engine::RetrievalEngine;

/// Bind a Unix socket at `socket_path`, accept connections until `stop` is
/// cancelled, and dispatch one request per connection.
pub async fn serve_unix(
    _engine: Arc<RetrievalEngine>,
    _socket_path: &Path,
    _stop: CancellationToken,
) -> crate::Result<()> {
    todo!("C.2: not yet implemented")
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
        assert!(resp.result.is_some(), "ping should return a result: {resp:?}");
        assert!(resp.error.is_none(), "ping must not return an error: {resp:?}");
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
        // Read the ack so the handler has a chance to flush before cancel.
        let resp_body = crate::ipc::read_frame(&mut conn).await.unwrap();
        let resp: Response = serde_json::from_slice(&resp_body).unwrap();
        assert!(resp.result.is_some(), "shutdown ack must carry a result: {resp:?}");
        // The listener task must terminate within 1 s of the Shutdown ack.
        let timeout = tokio::time::timeout(std::time::Duration::from_secs(1), task);
        let join_result = timeout.await.expect("server must stop within 1 s after Shutdown");
        join_result.expect("task must not panic").expect("serve_unix must return Ok");
    }
}
