//! Antigravity adapter (`agy -p`, stream-json output).
//!
//! Verified against `agy` 1.5.13 (`agy --help` and live runs).
//!
//! Quirks:
//! - The prompt MUST be one `-p=<prompt>` token. With `-p <prompt>` the
//!   flag swallows the next argument.
//! - Headless mode auto-denies shell permissions, hence
//!   `--dangerously-skip-permissions`.
//! - MCP is NOT available per session. `--settings` accepts only
//!   settings.json keys; `--settings '{"mcpServers":{...}}'` fails with
//!   `unknown setting "mcpServers"` (also `mcp`, `mcp_servers`). Servers are
//!   persistent global config (`agy mcp add`), which we must not mutate. A
//!   request with an `mcp_config` therefore logs a warning and runs without
//!   MCP; the supervisor records "decisions not narrated by agent".
//!
//! Events (one JSON object per line, discriminated by `event`): `init`,
//! `step_update`, and a final `result` with
//! `result.status` (`SUCCESS` or `ERROR`), `result.response` (final text) and
//! `result.error` on failure. Events carry token usage but no USD cost, so
//! `cost` is always `None`. A bad `--model` yields a `result` event with
//! status `ERROR` and exit code 1.

use async_trait::async_trait;
use ohara_core::task::RuntimeName;

use super::{AgentOutcome, AgentRequest, AgentRuntime, AgentStatus};
use crate::error::Result;

/// Runtime adapter that drives the `agy` CLI in headless (`-p`) mode.
pub struct AgyRuntime {
    name: RuntimeName,
    #[allow(dead_code)]
    model: Option<String>,
}

/// The fields we need from the final `{"event":"result",...}` line.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
struct ResultEvent {
    success: bool,
    /// Final response on success, error text on failure.
    message: String,
}

impl AgyRuntime {
    /// Build a runtime registered under `name`, optionally pinning `model`.
    pub fn new(name: RuntimeName, model: Option<String>) -> Self {
        Self { name, model }
    }

    fn argv(&self, _req: &AgentRequest) -> Vec<String> {
        Vec::new()
    }
}

fn parse_result(_tail: &str) -> Option<ResultEvent> {
    None
}

#[async_trait]
impl AgentRuntime for AgyRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
        Ok(AgentOutcome {
            status: AgentStatus::Crashed,
            final_message: req.prompt,
            cost: None,
            exit_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ohara_core::task::Deadline;
    use std::path::PathBuf;

    const FIXTURE: &str = include_str!("../../tests/fixtures/agy_stream.jsonl");

    fn req(model: Option<&str>) -> AgentRequest {
        AgentRequest {
            cwd: PathBuf::from("."),
            prompt: "do it".into(),
            mcp_config: None,
            deadline: Deadline(Utc::now() + chrono::Duration::seconds(60)),
            transcript: PathBuf::from("t.log"),
            model: model.map(String::from),
        }
    }

    fn rt(model: Option<&str>) -> AgyRuntime {
        AgyRuntime::new(RuntimeName("agy".into()), model.map(String::from))
    }

    #[test]
    fn prompt_is_single_equals_token() {
        let mut r = req(None);
        r.prompt = "--weird prompt with spaces".into();
        let a = rt(None).argv(&r);
        assert_eq!(a[0], "agy");
        assert_eq!(a[1], "-p=--weird prompt with spaces");
        assert!(!a.iter().any(|x| x == "-p"), "{a:?}");
        let pos = a.iter().position(|x| x == "--output-format").unwrap();
        assert_eq!(a[pos + 1], "stream-json");
    }

    #[test]
    fn skips_permissions_flag_present() {
        let a = rt(None).argv(&req(None));
        assert!(a.iter().any(|x| x == "--dangerously-skip-permissions"));
    }

    #[test]
    fn model_prefers_request_over_runtime_and_mcp_is_not_forwarded() {
        let a = rt(Some("a")).argv(&req(Some("b")));
        let i = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[i + 1], "b");
        let b = rt(Some("a")).argv(&req(None));
        let j = b.iter().position(|x| x == "--model").unwrap();
        assert_eq!(b[j + 1], "a");
        assert!(!rt(None).argv(&req(None)).iter().any(|x| x == "--model"));
        let mut r = req(None);
        r.mcp_config = Some(PathBuf::from("/tmp/mcp.json"));
        let c = rt(None).argv(&r);
        assert!(
            !c.iter().any(|x| x == "--settings" || x.contains("mcp")),
            "{c:?}"
        );
    }

    #[test]
    fn parses_result() {
        let ev = parse_result(FIXTURE).unwrap();
        assert!(ev.success);
        assert_eq!(ev.message, "ok");
    }

    #[test]
    fn parses_error_result() {
        let line = r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","response":"","error":"invalid model selection"}}"#;
        let ev = parse_result(line).unwrap();
        assert!(!ev.success);
        assert_eq!(ev.message, "invalid model selection");
        assert_eq!(parse_result("not json\n{\"event\":\"init\"}"), None);
    }

    #[tokio::test]
    #[ignore = "runs the real `agy` CLI; costs money"]
    async fn agy_live_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = req(None);
        r.cwd = dir.path().to_path_buf();
        r.prompt = "reply with the word ok".into();
        r.transcript = dir.path().join("t.log");
        r.deadline = Deadline(Utc::now() + chrono::Duration::seconds(180));
        let o = rt(None).run(r).await.unwrap();
        assert_eq!(o.status, AgentStatus::Completed, "{o:?}");
        assert!(o.final_message.to_lowercase().contains("ok"), "{o:?}");
    }
}
