//! Unix-socket client transport for the `ohara serve` daemon.

use crate::error::EngineError;
use crate::ipc::{read_frame, write_frame, Request, Response};
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;

/// Wall-clock budget for a single daemon round-trip (connect + write + read).
///
/// A hung or stalled daemon must not block the caller forever: `ohara query`
/// would hang, and an MCP agent's tool loop would stall behind it. 30s is
/// generous enough to cover a cold daemon servicing a heavy `find_pattern`
/// (embedding + rerank) yet short enough that a wedged daemon degrades to the
/// in-process fallback within a bounded, human-noticeable window.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A one-shot Unix-socket client for the `ohara serve` daemon.
///
/// Each [`call`][Client::call] opens a fresh connection, sends one request,
/// reads one response, and closes the stream.
pub struct Client {
    socket: std::path::PathBuf,
}

impl Client {
    /// Create a client pointed at `socket`.
    ///
    /// No I/O is performed here; the connection is established inside [`Self::call`].
    pub fn connect(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
        }
    }

    /// Send `req` to the daemon and return the parsed [`Response`].
    ///
    /// Opens a fresh connection per call, writes one length-prefixed frame,
    /// reads one length-prefixed frame back, and closes the connection. The
    /// whole round-trip is bounded by [`DEFAULT_CALL_TIMEOUT`]; on timeout this
    /// returns an [`EngineError`] (which `try_daemon_call` maps to `None`, so a
    /// stalled daemon transparently degrades to the in-process fallback).
    pub async fn call(&self, req: Request) -> crate::Result<Response> {
        self.call_with_timeout(req, DEFAULT_CALL_TIMEOUT).await
    }

    /// Like [`Self::call`] but with an explicit per-call timeout.
    ///
    /// The connect + write + read round-trip is wrapped in
    /// [`tokio::time::timeout`]. A timeout returns [`EngineError::Internal`]
    /// rather than blocking forever on a hung daemon.
    pub async fn call_with_timeout(
        &self,
        req: Request,
        timeout: Duration,
    ) -> crate::Result<Response> {
        match tokio::time::timeout(timeout, self.round_trip(req)).await {
            Ok(result) => result,
            Err(_) => Err(EngineError::Internal(format!(
                "daemon call timed out after {timeout:?} (socket {:?})",
                self.socket
            ))),
        }
    }

    /// The unbounded connect + write + read exchange, wrapped by the timeout in
    /// [`Self::call_with_timeout`].
    async fn round_trip(&self, req: Request) -> crate::Result<Response> {
        let mut conn = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| EngineError::Internal(format!("connect {:?}: {e}", self.socket)))?;
        let body =
            serde_json::to_vec(&req).map_err(|e| EngineError::Internal(format!("encode: {e}")))?;
        write_frame(&mut conn, &body).await?;
        let resp_body = read_frame(&mut conn).await?;
        let resp: Response = serde_json::from_slice(&resp_body)
            .map_err(|e| EngineError::Internal(format!("decode response: {e}")))?;
        Ok(resp)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::engine::tests::make_test_engine;
    use crate::ipc::{Request, RequestMethod};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn call_times_out_against_stalled_server() {
        use super::Client;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stalled.sock");
        // A listener that accepts connections but never replies: keep accepted
        // streams alive (so they don't close and trigger an EOF) and do nothing.
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => return,
                }
            }
        });

        let start = std::time::Instant::now();
        let result = Client::connect(&sock)
            .call_with_timeout(
                Request {
                    id: 1,
                    repo_path: None,
                    method: RequestMethod::Ping,
                },
                Duration::from_millis(150),
            )
            .await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "call against a stalled server must return Err, got: {result:?}"
        );
        // The call must not hang: it should return shortly after the timeout,
        // well under any wall-clock the harness would otherwise wait on.
        assert!(
            elapsed < Duration::from_secs(2),
            "call should return promptly after the timeout, took {elapsed:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn client_call_round_trips_ping() {
        use super::Client;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ohara.sock");
        let engine = Arc::new(make_test_engine());
        let stop = CancellationToken::new();
        let task = {
            let s = sock.clone();
            let stop = stop.clone();
            tokio::spawn(async move { crate::server::serve_unix(engine, &s, stop).await })
        };
        // Wait for socket.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let resp = Client::connect(&sock)
            .call(Request {
                id: 1,
                repo_path: None,
                method: RequestMethod::Ping,
            })
            .await
            .expect("call");
        assert!(resp.error.is_none(), "ping should succeed: {resp:?}");
        assert!(resp.result.is_some(), "ping should carry a result");
        stop.cancel();
        let _ = task.await;
    }
}
