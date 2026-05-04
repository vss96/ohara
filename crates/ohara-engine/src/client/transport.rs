//! Unix-socket client transport for the `ohara serve` daemon.

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
