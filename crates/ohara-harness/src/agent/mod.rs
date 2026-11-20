//! Agent runtimes: the `AgentRuntime` trait and its adapters.

use std::path::PathBuf;

use async_trait::async_trait;
use ohara_core::task::{CostCents, Deadline, RuntimeName};

use crate::error::Result;

/// Claude Code (`claude -p`) runtime adapter.
pub mod claude;
/// Codex (`codex exec --json`) runtime adapter.
pub mod codex;
/// Scripted runtime for tests.
#[cfg(any(test, feature = "test-util"))]
pub mod fake;
/// Config-driven generic runtime.
pub mod generic;
/// Deadline-bound child process streaming.
pub mod process;
/// Role-level runtime selection and failover.
pub mod roles;
/// Shared helpers for scanning JSON event streams.
pub mod stream;

/// One agent invocation.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// Working directory the agent runs in.
    pub cwd: PathBuf,
    /// Prompt text to deliver.
    pub prompt: String,
    /// MCP config file to hand the agent, if any.
    pub mcp_config: Option<PathBuf>,
    /// Instant after which the run is killed.
    pub deadline: Deadline,
    /// File the full output is written to.
    pub transcript: PathBuf,
    /// Model override for this run.
    pub model: Option<String>,
}

/// How an agent run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// The agent finished successfully.
    Completed,
    /// The deadline passed before the agent finished.
    TimedOut,
    /// The provider rate-limited or quota-capped the run.
    RateLimited,
    /// The agent was not authenticated.
    AuthFailed,
    /// The agent failed for any other reason.
    Crashed,
}

/// Result of one agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOutcome {
    /// How the run ended.
    pub status: AgentStatus,
    /// Last message or output tail from the agent.
    pub final_message: String,
    /// Reported cost, if the runtime exposes it.
    pub cost: Option<CostCents>,
    /// Process exit code, if one was observed.
    pub exit_code: Option<i32>,
}

/// A runnable agent backend.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Configured name of this runtime.
    fn name(&self) -> &RuntimeName;
    /// Run the agent once for `req`.
    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome>;
}

const RATE_LIMIT_MARKERS: [&str; 3] = ["rate limit", "429", "quota"];
const AUTH_MARKERS: [&str; 8] = [
    "not logged in",
    "please log in",
    "please login",
    "/login",
    "invalid api key",
    "unauthorized",
    "401",
    "authentication failed",
];

/// Classify a failed run from the tail of its output. Shared by all adapters.
pub fn classify_failure(stdout_tail: &str) -> AgentStatus {
    let lower = stdout_tail.to_lowercase();
    if RATE_LIMIT_MARKERS.iter().any(|m| lower.contains(m)) {
        return AgentStatus::RateLimited;
    }
    if AUTH_MARKERS.iter().any(|m| lower.contains(m)) {
        return AgentStatus::AuthFailed;
    }
    AgentStatus::Crashed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rate_limit_and_auth_strings() {
        assert_eq!(
            classify_failure("Error: Rate Limit exceeded"),
            AgentStatus::RateLimited
        );
        assert_eq!(
            classify_failure("HTTP 429 too many"),
            AgentStatus::RateLimited
        );
        assert_eq!(
            classify_failure("monthly QUOTA reached"),
            AgentStatus::RateLimited
        );
        assert_eq!(
            classify_failure("You are not logged in"),
            AgentStatus::AuthFailed
        );
        assert_eq!(classify_failure("Unauthorized"), AgentStatus::AuthFailed);
        assert_eq!(classify_failure("status 401"), AgentStatus::AuthFailed);
        assert_eq!(classify_failure("please login"), AgentStatus::AuthFailed);
        assert_eq!(classify_failure("segfault"), AgentStatus::Crashed);
        assert_eq!(classify_failure(""), AgentStatus::Crashed);
    }

    #[test]
    fn crash_mentioning_login_code_is_not_auth_failure() {
        assert_eq!(
            classify_failure("test auth::add_basic_login ... FAILED\npanicked"),
            AgentStatus::Crashed
        );
    }
}
