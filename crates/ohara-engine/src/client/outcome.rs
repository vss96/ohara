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
    fn rebuild_refusal_appends_the_remedy_once() {
        let msg = rebuild_refusal("index needs rebuild (x)");
        assert_eq!(msg.matches(REBUILD_REMEDY).count(), 1);
        assert!(msg.starts_with("index needs rebuild (x)"));
    }
}
