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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_function_and_class_from_ts() {
        let src = "function alpha(): number { return 1; }\n\
                   class Foo {\n  bar(x: number): number { return x; }\n}\n";
        let syms = extract("a.ts", src, "deadbeef", TsFlavor::Ts).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha missing: {names:?}");
        assert!(names.contains(&"Foo"), "Foo missing: {names:?}");
        assert!(names.contains(&"bar"), "bar missing: {names:?}");
        for s in &syms {
            assert_eq!(s.language, "typescript");
        }
    }
}
