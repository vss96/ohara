//! Typed outcome of a daemon round-trip and the single routing policy
//! every caller (CLI, MCP) applies to it.

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
