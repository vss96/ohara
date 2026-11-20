//! Codex adapter (`codex exec --json`).
use ohara_core::task::RuntimeName;

use super::{AgentRequest, AgentStatus};

pub struct CodexRuntime {
    name: RuntimeName,
    model: Option<String>,
}

impl CodexRuntime {
    pub fn new(name: RuntimeName, model: Option<String>) -> Self {
        Self { name, model }
    }
    fn argv(&self, _req: &AgentRequest, _mcp: &[String]) -> Vec<String> {
        vec![]
    }
}

fn mcp_overrides(_json: &str) -> crate::error::Result<Vec<String>> {
    Ok(vec![])
}

fn parse_final(_tail: &str) -> Option<(AgentStatus, String)> {
    None
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
                "codex", "exec", "--json", "--skip-git-repo-check", "-C", "/work", "--sandbox",
                "workspace-write", "do it"
            ]
        );
        let m = rt(Some("a")).argv(&req(Some("b")), &["x=1".into()]);
        assert_eq!(
            m,
            [
                "codex", "exec", "--json", "--skip-git-repo-check", "-C", "/work", "--sandbox",
                "workspace-write", "-m", "b", "-c", "x=1", "do it"
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
}
