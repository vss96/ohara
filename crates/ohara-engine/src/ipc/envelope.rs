//! IPC wire types: request/response envelopes for the `ohara serve` daemon.

use ohara_core::{explain::ExplainQuery, query::PatternQuery};
use serde::{Deserialize, Serialize};

/// Methods that can be dispatched to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum RequestMethod {
    Ping,
    Shutdown,
    FindPattern(PatternQuery),
    ExplainChange(ExplainQuery),
    InvalidateRepo,
    IndexStatus,
    Metrics,
}

/// A single IPC request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Caller-assigned correlation ID; echoed back in [`Response`].
    pub id: u64,
    /// Repository path. `None` for repo-agnostic methods (e.g. `Ping`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(flatten)]
    pub method: RequestMethod,
}

/// Structured error codes for [`ErrorPayload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotIndexed,
    NeedsRebuild,
    Internal,
    InvalidRequest,
    /// The requested method is recognised but not yet implemented in this
    /// version of the daemon. Callers should fall back to in-process logic
    /// rather than treating this as an unrecoverable error.
    NotImplemented,
}

/// Error payload embedded in a failed [`Response`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
}

/// A single IPC response frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Echoed correlation ID from the originating [`Request`].
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ohara_core::query::PatternQuery;

    #[test]
    fn ping_round_trip() {
        let req = Request {
            id: 42,
            repo_path: None,
            method: RequestMethod::Ping,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.method, RequestMethod::Ping));
        assert_eq!(back.id, 42);
    }

    #[test]
    fn find_pattern_round_trip() {
        let req = Request {
            id: 1,
            repo_path: Some("/repo".into()),
            method: RequestMethod::FindPattern(PatternQuery {
                query: "retry with backoff".into(),
                k: 5,
                language: None,
                since_unix: None,
                no_rerank: false,
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.method, RequestMethod::FindPattern(_)));
    }

    #[test]
    fn error_response_omits_result() {
        let resp = Response {
            id: 7,
            result: None,
            error: Some(ErrorPayload {
                code: ErrorCode::Internal,
                message: "something broke".into(),
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        // result must be absent from the JSON (skip_serializing_if)
        assert!(
            !json.contains("\"result\""),
            "result field should be omitted for error responses, got: {json}"
        );
        assert!(json.contains("\"error\""));
    }

    #[test]
    fn success_response_omits_error() {
        let resp = Response {
            id: 8,
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("\"error\""),
            "error field should be omitted for success responses, got: {json}"
        );
        assert!(json.contains("\"result\""));
    }
}
