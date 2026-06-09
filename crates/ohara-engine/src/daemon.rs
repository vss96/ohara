//! Long-lived daemon runner shared by `ohara serve` and `ohara-mcp serve`.
//!
//! Owns the socket listener plus the watchdogs (readiness, registry
//! heartbeat, whole-process idle exit, reranker idle unload). Binaries
//! construct the engine (or call [`run_daemon`] for the default CPU
//! providers) — keeping provider choice in one place per binary.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ipc::{Request, RequestMethod};
    use std::sync::Arc;

    use crate::engine::tests::make_test_engine;

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
