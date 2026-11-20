//! Claude Code adapter (`claude -p`, stream-json output).
//!
//! Flags verified against `claude --help` on Claude Code 2.1.329:
//! `-p/--print`, `--output-format stream-json` (needs `--verbose` in print
//! mode), `--permission-mode acceptEdits`, `--strict-mcp-config`,
//! `--mcp-config <configs...>`, `--allowedTools <tools...>` (accepts one
//! space-separated string) and `--model`. `--mcp-config` and `--allowedTools`
//! are variadic, so the prompt is placed right after `-p` and each variadic
//! flag is followed by another flag or ends the argv.

use std::path::Path;

use async_trait::async_trait;
use ohara_core::task::{CostCents, RuntimeName};

use super::process::run_streaming;
use super::stream::{last_json_line, last_json_line_in_file};
use super::{
    classify_error_result, classify_failure, AgentOutcome, AgentRequest, AgentRuntime, AgentStatus,
};
use crate::error::Result;

const ALLOWED_TOOLS: &str = "Bash Edit Write Read Glob Grep";

/// Runtime adapter that drives the `claude` CLI in headless (`-p`) mode and
/// maps its stream-json result event to an [`AgentOutcome`].
pub struct ClaudeCodeRuntime {
    name: RuntimeName,
    model: Option<String>,
}

/// The fields we need from the final `{"type":"result",...}` event.
#[derive(Debug, Clone, PartialEq)]
struct ResultEvent {
    is_error: bool,
    result: String,
    cost: Option<CostCents>,
}

/// Status for a parsed result event. A clean result alone is not success:
/// claude must also have exited 0.
fn result_status(ev: &ResultEvent, code: Option<i32>, tail: &str) -> AgentStatus {
    match (ev.is_error, code) {
        (false, Some(0)) => AgentStatus::Completed,
        (false, _) => classify_failure(tail),
        (true, _) => classify_error_result(&ev.result, tail),
    }
}

impl ClaudeCodeRuntime {
    /// Build a runtime registered under `name`, optionally pinning `model`.
    pub fn new(name: RuntimeName, model: Option<String>) -> Self {
        Self { name, model }
    }

    fn argv(&self, req: &AgentRequest) -> Vec<String> {
        let mut argv: Vec<String> = [
            "claude",
            "-p",
            req.prompt.as_str(),
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "acceptEdits",
            "--strict-mcp-config",
            "--allowedTools",
            ALLOWED_TOOLS,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(cfg) = &req.mcp_config {
            // Headless acceptEdits denies MCP tools that are not allow-listed.
            if let Some(slot) = argv.iter().position(|a| a == ALLOWED_TOOLS) {
                argv[slot] = format!(
                    "{ALLOWED_TOOLS} {}",
                    crate::decisions::propose_decision_tool_name()
                );
            }
            argv.push("--mcp-config".to_string());
            argv.push(cfg.display().to_string());
        }
        if let Some(model) = req.model.as_ref().or(self.model.as_ref()) {
            argv.push("--model".to_string());
            argv.push(model.clone());
        }
        argv
    }
}

/// Dollars to cents, rounded up. Negative or non-finite values become zero.
fn usd_to_cents(usd: f64) -> CostCents {
    if !usd.is_finite() || usd <= 0.0 {
        return CostCents(0);
    }
    // Round to micro-dollars first so float noise (0.07 * 100 = 7.000000000000001) does not add a cent.
    CostCents(((usd * 1_000_000.0).round() as u64).div_ceil(10_000))
}

fn pick_result(v: &serde_json::Value) -> Option<ResultEvent> {
    if v.get("type")?.as_str()? != "result" {
        return None;
    }
    Some(ResultEvent {
        is_error: v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false),
        result: v
            .get("result")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        cost: v
            .get("total_cost_usd")
            .and_then(|c| c.as_f64())
            .map(usd_to_cents),
    })
}

/// Scan `tail` from the last line backwards for the final result event.
fn parse_result(tail: &str) -> Option<ResultEvent> {
    last_json_line(tail, pick_result)
}

/// Result event from the capped tail, else from the full transcript (the
/// tail cuts a final line longer than the cap).
async fn find_result(tail: &str, transcript: &Path) -> Option<ResultEvent> {
    match parse_result(tail) {
        Some(ev) => Some(ev),
        None => last_json_line_in_file(transcript, pick_result).await,
    }
}

#[async_trait]
impl AgentRuntime for ClaudeCodeRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
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
                cost: parse_result(&exit.stdout_tail).and_then(|r| r.cost),
                final_message: exit.stdout_tail,
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
        let status = result_status(&ev, exit.code, &exit.stdout_tail);
        Ok(AgentOutcome {
            status,
            final_message: ev.result,
            cost: ev.cost,
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

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude_stream.jsonl");

    fn req(mcp: Option<&str>, model: Option<&str>) -> AgentRequest {
        AgentRequest {
            cwd: PathBuf::from("."),
            prompt: "do it".into(),
            mcp_config: mcp.map(PathBuf::from),
            deadline: Deadline(Utc::now() + chrono::Duration::seconds(60)),
            transcript: PathBuf::from("t.log"),
            model: model.map(String::from),
        }
    }

    fn rt(model: Option<&str>) -> ClaudeCodeRuntime {
        ClaudeCodeRuntime::new(RuntimeName("claude".into()), model.map(String::from))
    }

    #[test]
    fn argv_has_strict_mcp_and_stream_json() {
        let a = rt(None).argv(&req(Some("/tmp/mcp.json"), None));
        assert_eq!(&a[..3], ["claude", "-p", "do it"]);
        for flag in ["--verbose", "--strict-mcp-config", "--mcp-config"] {
            assert!(a.iter().any(|x| x == flag), "{flag} in {a:?}");
        }
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert_eq!(a[pos("--output-format") + 1], "stream-json");
        assert_eq!(a[pos("--permission-mode") + 1], "acceptEdits");
        assert_eq!(
            a[pos("--allowedTools") + 1],
            "Bash Edit Write Read Glob Grep mcp__ohara__propose_decision"
        );
        assert_eq!(a[pos("--mcp-config") + 1], "/tmp/mcp.json");
        assert!(!a.iter().any(|x| x == "--model"));
    }

    #[test]
    fn allowed_tools_include_propose_decision_when_mcp_configured() {
        let tools = |a: &[String]| {
            let i = a.iter().position(|x| x == "--allowedTools").unwrap();
            a[i + 1].clone()
        };
        let with = tools(&rt(None).argv(&req(Some("/tmp/mcp.json"), None)));
        assert!(with.starts_with("Bash Edit Write Read Glob Grep"));
        assert!(with.split(' ').any(|t| t == "mcp__ohara__propose_decision"));
        let without = tools(&rt(None).argv(&req(None, None)));
        assert!(!without.contains("propose_decision"), "{without}");
    }

    #[test]
    fn argv_model_prefers_request_over_runtime() {
        let a = rt(Some("sonnet")).argv(&req(None, Some("opus")));
        let i = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[i + 1], "opus");
        assert!(!a.iter().any(|x| x == "--mcp-config"));
        let b = rt(Some("sonnet")).argv(&req(None, None));
        let j = b.iter().position(|x| x == "--model").unwrap();
        assert_eq!(b[j + 1], "sonnet");
    }

    #[test]
    fn parses_result_event_cost_and_message() {
        let ev = parse_result(FIXTURE).unwrap();
        assert!(!ev.is_error);
        assert_eq!(ev.result, "ok");
        assert_eq!(ev.cost, Some(CostCents(27)));
    }

    #[test]
    fn parses_result_from_truncated_tail() {
        let tail = format!("rtial garbage line\n{FIXTURE}");
        assert_eq!(parse_result(&tail).unwrap().result, "ok");
        assert_eq!(parse_result("not json\n{\"type\":\"assistant\"}"), None);
    }

    #[test]
    fn cost_rounds_up() {
        assert_eq!(usd_to_cents(0.0), CostCents(0));
        assert_eq!(usd_to_cents(0.001), CostCents(1));
        assert_eq!(usd_to_cents(0.5), CostCents(50));
        assert_eq!(usd_to_cents(-1.0), CostCents(0));
    }

    #[test]
    fn cents_ignore_float_noise() {
        assert_eq!(usd_to_cents(0.07), CostCents(7));
        assert_eq!(usd_to_cents(0.0701), CostCents(8));
    }

    #[tokio::test]
    async fn oversized_result_line_is_read_from_transcript() {
        let big = "x".repeat(70 * 1024);
        let line = format!(
            r#"{{"type":"result","is_error":false,"result":"{big}","total_cost_usd":0.07}}"#
        );
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        std::fs::write(&log, format!("{FIXTURE}{line}\n")).unwrap();
        let tail = &line[line.len() - 64 * 1024..];
        assert!(parse_result(tail).is_none());
        let ev = find_result(tail, &log).await.unwrap();
        assert_eq!(ev.result, big);
        assert_eq!(ev.cost, Some(CostCents(7)));
        assert!(find_result(tail, &dir.path().join("missing.log"))
            .await
            .is_none());
    }

    #[test]
    fn error_result_classified() {
        let tail = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"API Error: 429 rate limit exceeded","total_cost_usd":0.0}"#;
        let ev = parse_result(tail).unwrap();
        assert!(ev.is_error);
        assert_eq!(classify_failure(&ev.result), AgentStatus::RateLimited);
        let auth =
            r#"{"type":"result","is_error":true,"result":"Invalid API key · Please run /login"}"#;
        let ev = parse_result(auth).unwrap();
        assert_eq!(classify_failure(&ev.result), AgentStatus::AuthFailed);
    }

    #[test]
    fn error_result_with_generic_text_uses_tail_for_classification() {
        let tail = "stderr: HTTP 429 rate limit exceeded";
        assert_eq!(classify_error_result("", tail), AgentStatus::RateLimited);
        assert_eq!(
            classify_error_result("error", tail),
            AgentStatus::RateLimited
        );
        assert_eq!(classify_error_result("error", "boom"), AgentStatus::Crashed);
    }

    #[test]
    fn claude_nonzero_exit_is_not_completed() {
        let ev = parse_result(FIXTURE).unwrap();
        assert_eq!(result_status(&ev, Some(0), ""), AgentStatus::Completed);
        assert_eq!(result_status(&ev, Some(1), ""), AgentStatus::Crashed);
        assert_eq!(result_status(&ev, None, ""), AgentStatus::Crashed);
        assert_eq!(
            result_status(&ev, Some(1), "HTTP 429"),
            AgentStatus::RateLimited
        );
    }

    #[tokio::test]
    #[ignore = "runs the real `claude` CLI; costs money"]
    async fn claude_live_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = req(None, None);
        r.cwd = dir.path().to_path_buf();
        r.prompt = "reply with the word ok".into();
        r.transcript = dir.path().join("t.log");
        r.deadline = Deadline(Utc::now() + chrono::Duration::seconds(180));
        let o = rt(None).run(r).await.unwrap();
        assert_eq!(o.status, AgentStatus::Completed, "{o:?}");
        assert!(o.final_message.to_lowercase().contains("ok"), "{o:?}");
        assert!(o.cost.is_some());
    }
}
