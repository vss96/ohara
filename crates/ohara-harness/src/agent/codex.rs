//! Codex adapter (`codex exec --json`).
//!
//! Flags verified against `codex exec --help` on codex-cli 0.163.0:
//! `--json` (JSONL events on stdout), `--skip-git-repo-check`, `-C/--cd <DIR>`,
//! `-s/--sandbox workspace-write`, `-m/--model` and `-c key=value` (dotted
//! path, value parsed as TOML). The prompt is the trailing positional
//! argument. stdin is null (run_streaming), so codex does not wait for input.
//!
//! MCP: the request's Claude-format JSON (`{"mcpServers":{name:{command,args,env}}}`)
//! is translated to `-c mcp_servers.<name>.command/args/env` overrides.
//!
//! Events used: `item.completed` with `item.type == "agent_message"` (final
//! message = the last one) and `turn.failed` (`error.message`). Other
//! `error` events and `item.completed` items of type `error` are transient
//! warnings and are ignored. We parse the events rather than using
//! `-o/--output-last-message`: the events are already in the transcript and
//! carry the failure case too. `turn.completed` only has token counts, never
//! a USD cost, so `cost` is always `None`.

use async_trait::async_trait;
use ohara_core::task::RuntimeName;

use super::process::run_streaming;
use super::stream::{last_json_line, last_json_line_in_file};
use super::{classify_failure, AgentOutcome, AgentRequest, AgentRuntime, AgentStatus};
use crate::error::{Error, Result};

pub struct CodexRuntime {
    name: RuntimeName,
    model: Option<String>,
}

/// Final state of a codex run: success with the last agent message, or the
/// `turn.failed` error message.
#[derive(Debug, Clone, PartialEq)]
enum Final {
    Message(String),
    Failed(String),
}

impl CodexRuntime {
    pub fn new(name: RuntimeName, model: Option<String>) -> Self {
        Self { name, model }
    }

    /// `mcp` is the already-translated `-c` override list (see [`mcp_overrides`]).
    fn argv(&self, req: &AgentRequest, mcp: &[String]) -> Vec<String> {
        let mut argv: Vec<String> = [
            "codex",
            "exec",
            "--json",
            "--skip-git-repo-check",
            "-C",
            &req.cwd.display().to_string(),
            "--sandbox",
            "workspace-write",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(model) = req.model.as_ref().or(self.model.as_ref()) {
            argv.push("-m".to_string());
            argv.push(model.clone());
        }
        argv.extend(mcp.iter().cloned());
        argv.push(req.prompt.clone());
        argv
    }
}

/// A TOML basic string. JSON string escapes are valid TOML escapes.
fn toml_str(s: &str) -> String {
    // serde_json leaves DEL (0x7f) raw, which TOML basic strings forbid.
    serde_json::Value::String(s.to_string())
        .to_string()
        .replace('\u{7f}', "\\u007f")
}

fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match bare {
        true => name.to_string(),
        false => toml_str(name),
    }
}

fn str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(toml_str)).collect())
        .unwrap_or_default()
}

/// Translate a Claude-format mcp config into codex `-c` override args.
fn mcp_overrides(json: &str) -> Result<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Runtime(format!("codex: invalid mcp config json: {e}")))?;
    let servers = v
        .get("mcpServers")
        .and_then(|s| s.as_object())
        .ok_or_else(|| Error::Runtime("codex: mcp config has no mcpServers object".into()))?;
    let mut out = Vec::new();
    for (name, spec) in servers {
        let command = spec
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| Error::Runtime(format!("codex: mcp server {name} has no command")))?;
        if name.contains('.') || name.contains('=') {
            return Err(Error::Runtime(format!(
                "codex: mcp server name {name:?} contains '.' or '=', which `codex -c` cannot address"
            )));
        }
        let prefix = format!("mcp_servers.{}", toml_key(name));
        out.push("-c".to_string());
        out.push(format!("{prefix}.command={}", toml_str(command)));
        let args = str_list(spec.get("args"));
        if !args.is_empty() {
            out.push("-c".to_string());
            out.push(format!("{prefix}.args=[{}]", args.join(",")));
        }
        let mut env = Vec::new();
        for (k, v) in spec
            .get("env")
            .and_then(|e| e.as_object())
            .into_iter()
            .flatten()
        {
            let val = v.as_str().ok_or_else(|| {
                Error::Runtime(format!("codex: mcp server {name} env {k} is not a string"))
            })?;
            env.push(format!("{}={}", toml_str(k), toml_str(val)));
        }
        if !env.is_empty() {
            out.push("-c".to_string());
            out.push(format!("{prefix}.env={{{}}}", env.join(",")));
        }
    }
    Ok(out)
}

fn pick_final(v: &serde_json::Value) -> Option<Final> {
    match v.get("type")?.as_str()? {
        "turn.failed" => {
            let msg = v.get("error")?.get("message")?.as_str()?;
            Some(Final::Failed(msg.to_string()))
        }
        "item.completed" => {
            let item = v.get("item")?;
            if item.get("type")?.as_str()? != "agent_message" {
                return None;
            }
            Some(Final::Message(item.get("text")?.as_str()?.to_string()))
        }
        _ => None,
    }
}

fn to_status(f: Final) -> (AgentStatus, String) {
    match f {
        Final::Message(m) => (AgentStatus::Completed, m),
        Final::Failed(m) => (classify_failure(&m), m),
    }
}

/// Last relevant event in `tail`, as a status plus message.
fn parse_final(tail: &str) -> Option<(AgentStatus, String)> {
    last_json_line(tail, pick_final).map(to_status)
}

/// Combine the parsed final event with the exit code. A message alone is not
/// success: codex must also have exited 0.
fn decide(
    found: Option<(AgentStatus, String)>,
    code: Option<i32>,
    tail: &str,
) -> (AgentStatus, String) {
    match found {
        Some((AgentStatus::Completed, msg)) if code != Some(0) => (classify_failure(tail), msg),
        Some(f) => f,
        None => (classify_failure(tail), tail.to_string()),
    }
}

#[async_trait]
impl AgentRuntime for CodexRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
        let mcp = match &req.mcp_config {
            Some(path) => mcp_overrides(&tokio::fs::read_to_string(path).await?)?,
            None => Vec::new(),
        };
        let exit = run_streaming(
            &self.argv(&req, &mcp),
            None,
            &req.cwd,
            req.deadline,
            &req.transcript,
        )
        .await?;
        let found = match parse_final(&exit.stdout_tail) {
            Some(f) => Some(f),
            None => last_json_line_in_file(&req.transcript, pick_final)
                .await
                .map(to_status),
        };
        if exit.killed_at_deadline {
            return Ok(AgentOutcome {
                status: AgentStatus::TimedOut,
                final_message: found.map(|f| f.1).unwrap_or(exit.stdout_tail),
                cost: None,
                exit_code: exit.code,
            });
        }
        let (status, final_message) = decide(found, exit.code, &exit.stdout_tail);
        Ok(AgentOutcome {
            status,
            final_message,
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

    const FIXTURE: &str = include_str!("../../tests/fixtures/codex_events.jsonl");

    fn req(model: Option<&str>) -> AgentRequest {
        AgentRequest {
            cwd: PathBuf::from("/work"),
            prompt: "do it".into(),
            mcp_config: None,
            deadline: Deadline(Utc::now() + chrono::Duration::seconds(60)),
            transcript: PathBuf::from("t.log"),
            model: model.map(String::from),
        }
    }
    fn rt(model: Option<&str>) -> CodexRuntime {
        CodexRuntime::new(RuntimeName("codex".into()), model.map(String::from))
    }

    #[test]
    fn argv_shape() {
        let a = rt(None).argv(&req(None), &[]);
        assert_eq!(
            a,
            [
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "/work",
                "--sandbox",
                "workspace-write",
                "do it"
            ]
        );
        let m = rt(Some("a")).argv(&req(Some("b")), &["-c".into(), "x=1".into()]);
        assert_eq!(
            m,
            [
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "/work",
                "--sandbox",
                "workspace-write",
                "-m",
                "b",
                "-c",
                "x=1",
                "do it"
            ]
        );
        assert_eq!(rt(Some("a")).argv(&req(None), &[])[9], "a");
    }

    #[test]
    fn mcp_translation() {
        let j = r#"{"mcpServers":{"ohara":{"command":"/bin/o","args":["serve","--x"],"env":{"K":"v"}}}}"#;
        assert_eq!(
            mcp_overrides(j).unwrap(),
            [
                "-c",
                r#"mcp_servers.ohara.command="/bin/o""#,
                "-c",
                r#"mcp_servers.ohara.args=["serve","--x"]"#,
                "-c",
                r#"mcp_servers.ohara.env={"K"="v"}"#,
            ]
        );
        assert!(mcp_overrides("not json").is_err());
        assert!(mcp_overrides(r#"{"mcpServers":{"a":{}}}"#).is_err());
    }

    #[test]
    fn parses_final_message() {
        assert_eq!(
            parse_final(FIXTURE),
            Some((AgentStatus::Completed, "ok".into()))
        );
        let warn = r#"{"type":"item.completed","item":{"type":"error","message":"429 warn"}}"#;
        let tail = format!("{FIXTURE}{warn}\n");
        assert_eq!(parse_final(&tail).unwrap().1, "ok");
    }

    #[test]
    fn error_event_classified() {
        let t = r#"{"type":"turn.started"}
{"type":"error","message":"x"}
{"type":"turn.failed","error":{"message":"429 rate limit exceeded"}}"#;
        assert_eq!(
            parse_final(t),
            Some((AgentStatus::RateLimited, "429 rate limit exceeded".into()))
        );
        let a = r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#;
        assert_eq!(parse_final(a).unwrap().0, AgentStatus::AuthFailed);
    }

    #[tokio::test]
    #[ignore = "runs the real `codex` CLI; needs login"]
    async fn codex_live_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = req(None);
        r.cwd = dir.path().to_path_buf();
        r.prompt = "reply with the word ok".into();
        r.transcript = dir.path().join("t.log");
        r.deadline = Deadline(Utc::now() + chrono::Duration::seconds(180));
        let o = rt(None).run(r).await.unwrap();
        assert_eq!(o.status, AgentStatus::Completed, "{o:?}");
        assert!(o.final_message.to_lowercase().contains("ok"), "{o:?}");
        assert_eq!(o.cost, None);
    }

    #[test]
    fn dotted_mcp_server_name_is_rejected() {
        let j = r#"{"mcpServers":{"a.b":{"command":"x"}}}"#;
        let e = mcp_overrides(j).unwrap_err().to_string();
        assert!(e.contains("a.b"), "{e}");
        let eq = r#"{"mcpServers":{"a=b":{"command":"x"}}}"#;
        assert!(mcp_overrides(eq).is_err());
    }

    #[test]
    fn spaced_mcp_server_name_is_quoted() {
        let j = r#"{"mcpServers":{"my srv":{"command":"x"}}}"#;
        assert_eq!(
            mcp_overrides(j).unwrap(),
            ["-c", r#"mcp_servers."my srv".command="x""#]
        );
    }

    #[test]
    fn non_string_env_value_is_rejected() {
        let j = r#"{"mcpServers":{"a":{"command":"x","env":{"K":1}}}}"#;
        assert!(mcp_overrides(j).is_err());
    }

    #[test]
    fn del_char_is_escaped() {
        assert_eq!(toml_str("a\u{7f}b"), "\"a\\u007fb\"");
    }

    #[test]
    fn message_then_nonzero_exit_is_not_completed() {
        let found = parse_final(FIXTURE);
        let (st, msg) = decide(found.clone(), Some(1), "boom");
        assert_eq!(st, AgentStatus::Crashed);
        assert_eq!(msg, "ok");
        assert_eq!(decide(found.clone(), None, "").0, AgentStatus::Crashed);
        assert_eq!(decide(found, Some(0), "").0, AgentStatus::Completed);
    }

    #[test]
    fn error_only_run_is_classified() {
        let tail = r#"{"type":"error","message":"429 rate limit hit"}
{"type":"item.completed","item":{"type":"error","message":"warn"}}"#;
        assert_eq!(parse_final(tail), None);
        assert_eq!(
            decide(parse_final(tail), Some(1), tail).0,
            AgentStatus::RateLimited
        );
    }
}
