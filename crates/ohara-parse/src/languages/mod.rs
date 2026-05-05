//! Per-language tree-sitter symbol extractors. Each module exposes
//! `extract(path, source, blob_sha) -> Result<Vec<Symbol>>` and is
//! dispatched by file extension in `crate::extract_for_path`.

pub mod java;
pub mod javascript;
pub mod kotlin;
pub mod python;
pub mod rust;
pub mod typescript;
