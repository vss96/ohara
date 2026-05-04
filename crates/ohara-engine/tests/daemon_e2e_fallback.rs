//! Plan-16 I.2: end-to-end fallback path. try_daemon_call must yield
//! None when the daemon socket is dead, so callers can drop to a
//! standalone in-process engine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ohara_engine::client::{try_daemon_call, DaemonHandle};
use ohara_engine::ipc::{Request, RequestMethod};
use std::path::PathBuf;

#[tokio::test]
async fn dead_socket_via_discover_yields_none() {
    let dead = PathBuf::from("/tmp/ohara-i2-dead-socket-xyz.sock");
    let _ = std::fs::remove_file(&dead);
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
    assert!(resp.is_none(), "dead socket must yield None");
}

#[tokio::test]
async fn discover_returning_error_yields_none() {
    let resp = try_daemon_call(
        || Err(ohara_engine::EngineError::Internal("simulated".into())),
        Request {
            id: 1,
            repo_path: None,
            method: RequestMethod::Ping,
        },
    )
    .await;
    assert!(resp.is_none(), "discover error must yield None");
}

#[tokio::test]
async fn discover_returning_none_yields_none() {
    let resp = try_daemon_call(
        || Ok(None),
        Request {
            id: 1,
            repo_path: None,
            method: RequestMethod::Ping,
        },
    )
    .await;
    assert!(resp.is_none(), "discover None must yield None");
}
