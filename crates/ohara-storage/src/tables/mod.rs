//! Per-table CRUD/query helpers for the SQLite storage backend.

pub mod commit;
pub mod embed_cache;
pub mod explain;
pub mod head_symbol_snapshot;
pub mod hunk;
pub mod hunk_symbol;
pub mod index_metadata;
pub mod indexed_tip;
pub mod prune;
pub mod repo;
pub mod symbol;
