//! Config-defined adapter for any agent CLI.

use async_trait::async_trait;
use ohara_core::task::RuntimeName;

use super::process::run_streaming;
use super::{classify_failure, AgentOutcome, AgentRequest, AgentRuntime, AgentStatus};
use crate::config::{DoneRule, PromptMode, RuntimeKind, RuntimeSpec};
use crate::error::{Error, Result};

const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Runtime that runs a user-configured command.
pub struct GenericRuntime {
    name: RuntimeName,
    command: Vec<String>,
    prompt: PromptMode,
    done: DoneRule,
}

impl GenericRuntime {
    /// Create a runtime from a `generic` spec; errors on another kind or a missing `command`.
    pub fn new(name: RuntimeName, spec: &RuntimeSpec) -> Result<Self> {
        if spec.kind != RuntimeKind::Generic {
            return Err(Error::Runtime(format!(
                "runtime {}: GenericRuntime needs kind \"generic\"",
                name.0
            )));
        }
        let command = spec
            .command
            .clone()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| Error::Runtime(format!("runtime {}: missing `command`", name.0)))?;
        Ok(Self {
            name,
            command,
            prompt: spec.prompt.unwrap_or(PromptMode::Arg),
            done: spec.done.clone().unwrap_or(DoneRule {
                exit_code: 0,
                output_contains: None,
            }),
        })
    }

    fn argv(&self, prompt: &str) -> Vec<String> {
        match self.prompt {
            PromptMode::Stdin => self.command.clone(),
            PromptMode::Arg => self
                .command
                .iter()
                .map(|a| a.replace(PROMPT_PLACEHOLDER, prompt))
                .collect(),
        }
    }

    fn is_done(&self, code: Option<i32>, tail: &str) -> bool {
        if code != Some(self.done.exit_code) {
            return false;
        }
        self.done
            .output_contains
            .as_ref()
            .is_none_or(|needle| tail.contains(needle.as_str()))
    }
}

#[async_trait]
impl AgentRuntime for GenericRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
        let stdin = match self.prompt {
            PromptMode::Stdin => Some(req.prompt.clone()),
            PromptMode::Arg => None,
        };
        let exit = run_streaming(
            &self.argv(&req.prompt),
            stdin,
            &req.cwd,
            req.deadline,
            &req.transcript,
        )
        .await?;
        let status = match (
            exit.killed_at_deadline,
            self.is_done(exit.code, &exit.stdout_tail),
        ) {
            (true, _) => AgentStatus::TimedOut,
            (false, true) => AgentStatus::Completed,
            (false, false) => classify_failure(&exit.stdout_tail),
        };
        Ok(AgentOutcome {
            status,
            final_message: exit.stdout_tail,
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
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn fixture() -> String {
        format!(
            "{}/tests/fixtures/fake_agent.sh",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    fn spec(command: Vec<String>, prompt: PromptMode, done: Option<DoneRule>) -> RuntimeSpec {
        RuntimeSpec {
            kind: RuntimeKind::Generic,
            model: None,
            command: Some(command),
            prompt: Some(prompt),
            done,
        }
    }

    fn req(dir: &Path, prompt: &str, secs: i64) -> AgentRequest {
        AgentRequest {
            cwd: dir.to_path_buf(),
            prompt: prompt.to_string(),
            mcp_config: None,
            deadline: Deadline(Utc::now() + chrono::Duration::seconds(secs)),
            transcript: dir.join("transcript.log"),
            model: None,
        }
    }

    fn rt(command: Vec<String>, prompt: PromptMode, done: Option<DoneRule>) -> GenericRuntime {
        GenericRuntime::new(RuntimeName("g".into()), &spec(command, prompt, done)).unwrap()
    }

    fn cmd(mode: &str, out: &Path, extra: &[&str]) -> Vec<String> {
        let mut v = vec!["sh".to_string(), fixture(), mode.to_string()];
        v.push(out.display().to_string());
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    #[tokio::test]
    async fn generic_arg_prompt_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let out: PathBuf = dir.path().join("argv.txt");
        let r = rt(
            cmd("argv", &out, &["--p", "{prompt}"]),
            PromptMode::Arg,
            None,
        );
        let o = r.run(req(dir.path(), "do the thing", 30)).await.unwrap();
        assert_eq!(o.status, AgentStatus::Completed);
        assert_eq!(o.exit_code, Some(0));
        assert_eq!(o.cost, None);
        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("do the thing"), "{argv}");
        assert!(!argv.contains("{prompt}"), "{argv}");
    }

    #[tokio::test]
    async fn generic_stdin_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stdin.txt");
        let r = rt(cmd("stdin", &out, &[]), PromptMode::Stdin, None);
        let o = r.run(req(dir.path(), "from stdin", 30)).await.unwrap();
        assert_eq!(o.status, AgentStatus::Completed);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "from stdin");
    }

    #[tokio::test]
    async fn deadline_kills_child_within_5s() {
        let dir = tempfile::tempdir().unwrap();
        let r = rt(
            cmd("sleep", &dir.path().join("x"), &[]),
            PromptMode::Arg,
            None,
        );
        let start = Instant::now();
        let o = r.run(req(dir.path(), "p", 1)).await.unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(7),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(o.status, AgentStatus::TimedOut);
    }

    #[tokio::test]
    async fn done_rule_output_contains_and_failure_classification() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o");
        let done = Some(DoneRule {
            exit_code: 0,
            output_contains: Some("ALL DONE".into()),
        });
        let ok = rt(
            cmd("say", &out, &["ALL DONE"]),
            PromptMode::Arg,
            done.clone(),
        );
        assert_eq!(
            ok.run(req(dir.path(), "p", 30)).await.unwrap().status,
            AgentStatus::Completed
        );
        let missing = rt(cmd("say", &out, &["nope"]), PromptMode::Arg, done);
        assert_eq!(
            missing.run(req(dir.path(), "p", 30)).await.unwrap().status,
            AgentStatus::Crashed
        );
        let limited = rt(
            cmd("fail", &out, &["429 rate limit"]),
            PromptMode::Arg,
            None,
        );
        let o = limited.run(req(dir.path(), "p", 30)).await.unwrap();
        assert_eq!(o.status, AgentStatus::RateLimited);
        assert_eq!(o.exit_code, Some(3));
    }

    #[test]
    fn rejects_non_generic_or_commandless_spec() {
        let mut s = spec(vec!["x".into()], PromptMode::Arg, None);
        s.command = None;
        assert!(GenericRuntime::new(RuntimeName("g".into()), &s).is_err());
        s.kind = RuntimeKind::Codex;
        s.command = Some(vec!["x".into()]);
        assert!(GenericRuntime::new(RuntimeName("g".into()), &s).is_err());
    }
}
