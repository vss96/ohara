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

use super::process::run_streaming;
use super::stream::{last_json_line, last_json_line_in_file};
use super::{
    classify_error_result, classify_failure, AgentOutcome, AgentRequest, AgentRuntime, AgentStatus,
};
use crate::error::Result;

/// Runtime adapter that drives the `agy` CLI in headless (`-p`) mode.
pub struct AgyRuntime {
    name: RuntimeName,
    model: Option<String>,
}

/// The fields we need from the final `{"event":"result",...}` line.
#[derive(Debug, Clone, PartialEq)]
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

    fn argv(&self, req: &AgentRequest) -> Vec<String> {
        let mut argv: Vec<String> = [
            "agy",
            // One token: a separate `-p <prompt>` swallows the next flag.
            &format!("-p={}", req.prompt),
            "--output-format",
            "stream-json",
            // Headless mode auto-denies shell permissions otherwise.
            "--dangerously-skip-permissions",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(model) = req.model.as_ref().or(self.model.as_ref()) {
            argv.push("--model".to_string());
            argv.push(model.clone());
        }
        argv
    }
}

fn pick_result(v: &serde_json::Value) -> Option<ResultEvent> {
    if v.get("event")?.as_str()? != "result" {
        return None;
    }
    let r = v.get("result")?;
    let text = |key: &str| r.get(key).and_then(|s| s.as_str()).unwrap_or_default();
    let success = r.get("status").and_then(|s| s.as_str()) == Some("SUCCESS");
    let message = match success {
        true => text("response"),
        false => match text("error") {
            "" => text("response"),
            error => error,
        },
    };
    Some(ResultEvent {
        success,
        message: message.trim_end().to_string(),
    })
}

/// Scan `tail` from the last line backwards for the final result event.
fn parse_result(tail: &str) -> Option<ResultEvent> {
    last_json_line(tail, pick_result)
}

/// Result event from the capped tail, else from the full transcript.
async fn find_result(tail: &str, transcript: &std::path::Path) -> Option<ResultEvent> {
    match parse_result(tail) {
        Some(ev) => Some(ev),
        None => last_json_line_in_file(transcript, pick_result).await,
    }
}

/// A `SUCCESS` result is only success when agy also exited 0.
fn result_status(ev: &ResultEvent, code: Option<i32>, tail: &str) -> AgentStatus {
    match (ev.success, code) {
        (true, Some(0)) => AgentStatus::Completed,
        (true, _) => classify_failure(tail),
        (false, _) => classify_error_result(&ev.message, tail),
    }
}

#[async_trait]
impl AgentRuntime for AgyRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
        if let Some(cfg) = &req.mcp_config {
            tracing::warn!(
                mcp_config = %cfg.display(),
                "agy cannot take MCP servers per session; running without them"
            );
        }
        let exit = run_streaming(
            &self.argv(&req),
            None,
            &req.cwd,
            req.deadline,
            &req.transcript,
        )
        .await?;
        if exit.killed_at_deadline {
            return Ok(AgentOutcome {
                status: AgentStatus::TimedOut,
                final_message: exit.stdout_tail,
                cost: None,
                exit_code: exit.code,
            });
        }
        let Some(ev) = find_result(&exit.stdout_tail, &req.transcript).await else {
            return Ok(AgentOutcome {
                status: classify_failure(&exit.stdout_tail),
                final_message: exit.stdout_tail,
                cost: None,
                exit_code: exit.code,
            });
        };
        Ok(AgentOutcome {
            status: result_status(&ev, exit.code, &exit.stdout_tail),
            final_message: ev.message,
            cost: None,
            exit_code: exit.code,
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

    #[test]
    fn nonzero_exit_is_not_completed() {
        let ev = parse_result(FIXTURE).unwrap();
        assert_eq!(result_status(&ev, Some(0), ""), AgentStatus::Completed);
        assert_eq!(result_status(&ev, Some(1), ""), AgentStatus::Crashed);
        assert_eq!(
            result_status(&ev, None, "HTTP 429"),
            AgentStatus::RateLimited
        );
    }

    #[test]
    fn error_result_is_classified_from_text_then_tail() {
        let ev = ResultEvent {
            success: false,
            message: "quota exceeded".into(),
        };
        assert_eq!(result_status(&ev, Some(1), ""), AgentStatus::RateLimited);
        let generic = ResultEvent {
            success: false,
            message: String::new(),
        };
        assert_eq!(
            result_status(&generic, Some(1), "401 unauthorized"),
            AgentStatus::AuthFailed
        );
        assert_eq!(
            result_status(&generic, Some(1), "boom"),
            AgentStatus::Crashed
        );
    }

    #[tokio::test]
    async fn oversized_result_line_is_read_from_transcript() {
        let big = "x".repeat(70 * 1024);
        let line =
            format!(r#"{{"event":"result","result":{{"status":"SUCCESS","response":"{big}"}}}}"#);
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        std::fs::write(&log, format!("{FIXTURE}{line}\n")).unwrap();
        let tail = &line[line.len() - 64 * 1024..];
        assert!(parse_result(tail).is_none());
        assert_eq!(find_result(tail, &log).await.unwrap().message, big);
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
