//! Outcome harness: supervisor, agent runtimes, guards, and config.

/// Agent runtimes and failover.
pub mod agent;
/// Harness configuration.
pub mod config;
/// Frozen task contract: model, freeze, and check runner.
pub mod contract;
/// Decision settlement and the agent-facing MCP config.
pub mod decisions;
/// Contract drafting from a workflow template.
pub mod draft;
/// Error type.
pub mod error;
/// Git CLI helper for task worktrees and refs.
pub mod git;
/// Guard: protected paths and test weakening.
pub mod guard;
/// Workflows: personal task templates with starters.
pub mod workflow;

pub use error::{Error, Result};
