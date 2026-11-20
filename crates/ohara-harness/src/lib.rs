//! Outcome harness: supervisor, agent runtimes, guards, and config.

/// Agent runtimes and failover.
pub mod agent;
/// Harness configuration.
pub mod config;
/// Error type.
pub mod error;

pub use error::{Error, Result};
