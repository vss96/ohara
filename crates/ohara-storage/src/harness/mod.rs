//! `harness.sqlite`: the outcome-harness task log.
//!
//! One database for all tasks, with its own append-only migration set in
//! `harness_migrations/` (see `migrations`), separate from the per-repo
//! index schema.

pub(crate) mod migrations;
mod rows;
mod task_store;

pub use task_store::SqliteTaskStore;
