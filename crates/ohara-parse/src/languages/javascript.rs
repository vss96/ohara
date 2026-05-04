use anyhow::Result;
use ohara_core::types::Symbol;

const QUERY_SRC: &str = include_str!("../../queries/javascript.scm");

pub fn extract(_file_path: &str, _source: &str, _blob_sha: &str) -> Result<Vec<Symbol>> {
    // Implemented in Phase 2.
    let _ = QUERY_SRC;
    Ok(vec![])
}
