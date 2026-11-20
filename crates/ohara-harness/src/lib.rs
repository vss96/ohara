//! Outcome harness: supervisor, agent runtimes, guards, and config.

/// Agent runtimes and failover.
pub mod agent;
/// Harness configuration.
pub mod config;
/// Frozen task contract: model, freeze, and check runner.
pub mod contract;
/// Error type.
pub mod error;
/// Git CLI helper for task worktrees and refs.
pub mod git;

pub use error::{Error, Result};
