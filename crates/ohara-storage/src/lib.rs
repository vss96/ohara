//! SQLite + sqlite-vec implementation of `ohara_core::Storage`.

pub mod codec;
pub mod metrics;
pub mod migrations;
pub mod storage_impl;
pub mod tables;

pub use storage_impl::SqliteStorage;
