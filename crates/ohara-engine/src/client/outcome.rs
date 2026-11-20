//! Typed outcome of a daemon round-trip and the single routing policy
//! every caller (CLI, MCP) applies to it.
//!
//! [`try_daemon_call`](crate::client::try_daemon_call) reports *what
//! happened* as a [`DaemonCallResult`]; [`DaemonCallResult::route`]
//! decides *what to do about it* as a [`DaemonRoute`]. Callers only
//! match on the route, so the "surface `NeedsRebuild`, otherwise fall
//! back in-process" policy and the rebuild refusal wording live here.

use crate::ipc::{ErrorCode, ErrorPayload};
use serde::de::DeserializeOwned;

/// The command that fixes a `NeedsRebuild` index, phrased for the user.
pub const REBUILD_REMEDY: &str = "Run `ohara index --rebuild` in this repo first.";

/// `detail` (e.g. the daemon's "index needs rebuild: <reason>") followed
/// by [`REBUILD_REMEDY`]. Shared by the CLI and MCP refusals.
pub fn rebuild_refusal(detail: &str) -> String {
    format!("{detail}. {REBUILD_REMEDY}")
}

/// What happened when routing a request through the daemon.
#[derive(Debug)]
pub enum DaemonCallResult {
    /// The daemon answered successfully; carries the `result` value.
    Ok(serde_json::Value),
    /// No daemon to talk to: disabled (`--no-daemon`, thin-client opt-out)
    /// or discovery found / spawned none.
    Unavailable,
    /// Discovery, connect, or transport failed — or the daemon sent a
    /// response with neither `result` nor `error`.
    ConnectFailed(String),
    /// The daemon answered with an error payload.
    DaemonErrored(ErrorPayload),
    /// The request was sent but no usable reply came back (timeout,
    /// dropped connection, missing or unreadable reply). The daemon may
    /// still be working on it. Produced by
    /// [`try_daemon_write`](crate::client::try_daemon_write).
    Interrupted(String),
}

/// What the caller should do with a [`DaemonCallResult`].
#[derive(Debug)]
pub enum DaemonRoute<T> {
    /// Use this daemon-served value.
    Served(T),
    /// Refuse with this user-facing message; do NOT fall back — the index
    /// is definitively unusable, so the in-process path can't do better.
    Refuse(String),
    /// Run the request in-process. The in-process engine carries the same
    /// `NeedsRebuild` guard, so it either refuses for the same reason or
    /// answers.
    Fallback,
}

impl DaemonCallResult {
    /// Apply the routing policy (issues #78, #96):
    /// - success with a decodable `T` → [`DaemonRoute::Served`];
    /// - `NeedsRebuild` → [`DaemonRoute::Refuse`] with [`rebuild_refusal`];
    /// - any other daemon error (incl. `NotImplemented`), unavailability,
    ///   transport failure, or an undecodable result → [`DaemonRoute::Fallback`].
    pub fn route<T: DeserializeOwned>(self) -> DaemonRoute<T> {
        match self {
            Self::Ok(value) => match serde_json::from_value(value) {
                Ok(v) => DaemonRoute::Served(v),
                Err(e) => {
                    tracing::warn!(error = %e, "undecodable daemon result; falling back in-process");
                    DaemonRoute::Fallback
                }
            },
            Self::DaemonErrored(err) if err.code == ErrorCode::NeedsRebuild => {
                DaemonRoute::Refuse(rebuild_refusal(&err.message))
            }
            Self::DaemonErrored(err) => {
                tracing::debug!(code = ?err.code, message = %err.message, "daemon error; falling back in-process");
                DaemonRoute::Fallback
            }
            Self::Unavailable | Self::ConnectFailed(_) | Self::Interrupted(_) => {
                DaemonRoute::Fallback
            }
        }
    }

    /// The routing policy for requests that write the index (plan-33:
    /// `EnsureIndexed`, `PruneUnreachable`). An in-process run while the
    /// daemon may still be writing would be a second writer, so:
    /// - [`DaemonRoute::Fallback`] only when the request provably never
    ///   reached the daemon (`Unavailable`, `ConnectFailed`) or the daemon
    ///   declined with one of `fallback_codes` (e.g. `NotIndexed` for an
    ///   index, which the daemon never builds from scratch);
    /// - `NeedsRebuild` → [`DaemonRoute::Refuse`] with [`rebuild_refusal`];
    /// - any other daemon error, an [`Self::Interrupted`] exchange, or an
    ///   undecodable result → [`DaemonRoute::Refuse`].
    pub fn route_write<T: DeserializeOwned>(self, fallback_codes: &[ErrorCode]) -> DaemonRoute<T> {
        match self {
            Self::Ok(value) => match serde_json::from_value(value) {
                Ok(v) => DaemonRoute::Served(v),
                Err(e) => DaemonRoute::Refuse(format!(
                    "undecodable daemon response: {e}; it may still be working"
                )),
            },
            Self::DaemonErrored(err) if err.code == ErrorCode::NeedsRebuild => {
                DaemonRoute::Refuse(rebuild_refusal(&err.message))
            }
            Self::DaemonErrored(err) if fallback_codes.contains(&err.code) => {
                tracing::debug!(code = ?err.code, "daemon declined; running in-process");
                DaemonRoute::Fallback
            }
            Self::DaemonErrored(err) => {
                DaemonRoute::Refuse(format!("daemon failed: {}", err.message))
            }
            Self::Interrupted(e) => DaemonRoute::Refuse(format!(
                "daemon did not finish ({e}); it may still be working. Wait for it \
                 (see `ohara status`) and retry; use --no-daemon only once it has finished"
            )),
            Self::Unavailable | Self::ConnectFailed(_) => DaemonRoute::Fallback,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ipc::{ErrorCode, ErrorPayload};

    fn errored(code: ErrorCode) -> DaemonCallResult {
        DaemonCallResult::DaemonErrored(ErrorPayload {
            code,
            message: "index needs rebuild: embedding_model mismatch".into(),
        })
    }

    #[test]
    fn ok_with_decodable_value_is_served() {
        let r = DaemonCallResult::Ok(serde_json::json!(7)).route::<u32>();
        assert!(matches!(r, DaemonRoute::Served(7)));
    }

    #[test]
    fn ok_with_undecodable_value_falls_back() {
        let r = DaemonCallResult::Ok(serde_json::json!("x")).route::<u32>();
        assert!(matches!(r, DaemonRoute::Fallback));
    }

    #[test]
    fn needs_rebuild_is_refused_with_remedy() {
        let DaemonRoute::Refuse(msg) = errored(ErrorCode::NeedsRebuild).route::<u32>() else {
            panic!("NeedsRebuild must refuse");
        };
        assert!(msg.contains("needs rebuild"), "got: {msg}");
        assert!(msg.contains("ohara index --rebuild"), "got: {msg}");
    }

    #[test]
    fn other_daemon_errors_fall_back() {
        for code in [
            ErrorCode::NotImplemented,
            ErrorCode::Internal,
            ErrorCode::NotIndexed,
            ErrorCode::InvalidRequest,
        ] {
            assert!(matches!(
                errored(code).route::<u32>(),
                DaemonRoute::Fallback
            ));
        }
    }

    #[test]
    fn unavailable_and_connect_failures_fall_back() {
        assert!(matches!(
            DaemonCallResult::Unavailable.route::<u32>(),
            DaemonRoute::Fallback
        ));
        assert!(matches!(
            DaemonCallResult::ConnectFailed("boom".into()).route::<u32>(),
            DaemonRoute::Fallback
        ));
    }

    #[test]
    fn interrupted_read_falls_back() {
        let r = DaemonCallResult::Interrupted("timed out".into()).route::<u32>();
        assert!(matches!(r, DaemonRoute::Fallback));
    }

    #[test]
    fn write_falls_back_only_before_send_or_on_listed_codes() {
        let not_indexed = || {
            DaemonCallResult::DaemonErrored(ErrorPayload {
                code: ErrorCode::NotIndexed,
                message: "repo not indexed".into(),
            })
        };
        assert!(matches!(
            DaemonCallResult::Unavailable.route_write::<u32>(&[]),
            DaemonRoute::Fallback
        ));
        assert!(matches!(
            DaemonCallResult::ConnectFailed("connect".into()).route_write::<u32>(&[]),
            DaemonRoute::Fallback
        ));
        assert!(matches!(
            not_indexed().route_write::<u32>(&[ErrorCode::NotIndexed]),
            DaemonRoute::Fallback
        ));
        assert!(matches!(
            not_indexed().route_write::<u32>(&[]),
            DaemonRoute::Refuse(_)
        ));
    }

    #[test]
    fn write_refuses_everything_after_send() {
        let DaemonRoute::Refuse(msg) =
            DaemonCallResult::Interrupted("timed out".into()).route_write::<u32>(&[])
        else {
            panic!("a write interrupted after send must refuse");
        };
        assert!(
            msg.contains("timed out") && msg.contains("may still be"),
            "{msg}"
        );

        let DaemonRoute::Refuse(msg) = DaemonCallResult::DaemonErrored(ErrorPayload {
            code: ErrorCode::Internal,
            message: "boom".into(),
        })
        .route_write::<u32>(&[ErrorCode::NotIndexed]) else {
            panic!("a daemon error on a write must refuse");
        };
        assert!(msg.contains("boom"), "{msg}");

        let DaemonRoute::Refuse(msg) =
            errored(ErrorCode::NeedsRebuild).route_write::<u32>(&[ErrorCode::NotIndexed])
        else {
            panic!("NeedsRebuild must refuse");
        };
        assert!(msg.contains(REBUILD_REMEDY), "{msg}");

        assert!(matches!(
            DaemonCallResult::Ok(serde_json::json!("x")).route_write::<u32>(&[]),
            DaemonRoute::Refuse(_)
        ));
        assert!(matches!(
            DaemonCallResult::Ok(serde_json::json!(7)).route_write::<u32>(&[]),
            DaemonRoute::Served(7)
        ));
    }

    #[test]
    fn an_interrupted_write_says_to_wait_before_running_in_process() {
        let r = DaemonCallResult::Interrupted("reply lost".into()).route_write::<u32>(&[]);
        let DaemonRoute::Refuse(msg) = r else {
            panic!("an interrupted write must be refused");
        };
        assert!(msg.contains("ohara status"), "{msg}");
        assert!(
            msg.contains("only once it has finished"),
            "--no-daemon must not be offered as an immediate retry: {msg}"
        );
    }

    #[test]
    fn rebuild_refusal_appends_the_remedy_once() {
        let msg = rebuild_refusal("index needs rebuild (x)");
        assert_eq!(msg.matches(REBUILD_REMEDY).count(), 1);
        assert!(msg.starts_with("index needs rebuild (x)"));
    }
}
