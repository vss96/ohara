//! Unix-socket client transport for the `ohara serve` daemon.

use crate::error::EngineError;
use crate::ipc::{read_frame, write_frame, Request, Response};
use std::path::Path;
use tokio::net::UnixStream;

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
    /// No I/O is performed here; the connection is established inside [`call`].
    pub fn connect(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
        }
    }

    /// Send `req` to the daemon and return the parsed [`Response`].
    ///
    /// Opens a fresh connection per call, writes one length-prefixed frame,
    /// reads one length-prefixed frame back, and closes the connection.
    pub async fn call(&self, req: Request) -> crate::Result<Response> {
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
    use tokio_util::sync::CancellationToken;

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
