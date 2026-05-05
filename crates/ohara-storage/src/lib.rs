//! SQLite + sqlite-vec implementation of [`ohara_core::Storage`].
//!
//! This crate is the only place in the workspace that constructs SQL
//! (CONTRIBUTING §1). Schema lives in `migrations/` and is applied via
//! `refinery` — migrations are append-only; never edit one that has
//! shipped.
//!
//! Layout:
//! - [`codec`]: little-endian f32 byte codec for sqlite-vec `FLOAT[N]`
//!   columns.
//! - [`migrations`]: refinery wrapper that runs `migrations/*.sql` on
//!   open.
//! - [`storage_impl`]: the `Storage` impl ([`SqliteStorage`]). Resume-safe:
//!   per-commit writes are DELETE-then-INSERT into `vec_*` and `fts_*`
//!   so a crashed-mid-commit run replays cleanly.
//! - [`tables`]: per-table query helpers (commit, hunk, symbol, vec_*,
//!   fts_*, index_metadata).
//! - [`metrics`]: lightweight counters for index/query timing.
//!
//! [`ohara_core::Storage`]: ../ohara_core/storage/trait.Storage.html

pub mod codec;
pub mod metrics;
pub mod migrations;
pub mod storage_impl;
pub mod tables;

pub use storage_impl::SqliteStorage;
