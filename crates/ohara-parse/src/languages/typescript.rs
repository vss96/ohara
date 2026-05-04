use anyhow::Result;
use ohara_core::types::Symbol;

const QUERY_SRC: &str = include_str!("../../queries/typescript.scm");

/// Discriminator for the two grammar handles inside `tree-sitter-typescript`:
/// `LANGUAGE_TYPESCRIPT` parses `.ts`; `LANGUAGE_TSX` parses `.tsx`.
#[derive(Debug, Clone, Copy)]
pub enum TsFlavor {
    Ts,
    Tsx,
}

pub fn extract(
    _file_path: &str,
    _source: &str,
    _blob_sha: &str,
    _flavor: TsFlavor,
) -> Result<Vec<Symbol>> {
    // Implemented in Phase 3.
    let _ = QUERY_SRC;
    Ok(vec![])
}
