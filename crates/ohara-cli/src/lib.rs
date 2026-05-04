//! ohara-cli: the `ohara` binary.
//!
//! Subcommands: `init`, `index`, `query`, `status`, `explain`, `update`, `serve`, `daemon`.
//! Each lives in its own module under `commands/`.
pub mod commands;
pub mod perf_trace;
pub mod progress;
pub mod resources;
