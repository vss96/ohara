//! Agent runtimes: the `AgentRuntime` trait and its adapters.

use std::path::PathBuf;

use async_trait::async_trait;
use ohara_core::task::{CostCents, Deadline, RuntimeName};

use crate::error::Result;

#[cfg(any(test, feature = "test-util"))]
pub mod fake;
pub mod generic;
pub mod process;

/// One agent invocation.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub cwd: PathBuf,
    pub prompt: String,
    pub mcp_config: Option<PathBuf>,
    pub deadline: Deadline,
    pub transcript: PathBuf,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Completed,
    TimedOut,
    RateLimited,
    AuthFailed,
    Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOutcome {
    pub status: AgentStatus,
    pub final_message: String,
    pub cost: Option<CostCents>,
    pub exit_code: Option<i32>,
}

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    fn name(&self) -> &RuntimeName;
    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome>;
}

const RATE_LIMIT_MARKERS: [&str; 3] = ["rate limit", "429", "quota"];
const AUTH_MARKERS: [&str; 4] = ["not logged in", "unauthorized", "401", "login"];

/// Classify a failed run from the tail of its output. Shared by all adapters.
pub fn classify_failure(stdout_tail: &str) -> AgentStatus {
    let _ = (stdout_tail, RATE_LIMIT_MARKERS, AUTH_MARKERS);
    unimplemented!("red")
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
}
