//! Java symbol extractor (Plan 4).
//!
//! Walks a tree-sitter-java parse tree and emits one `Symbol` per
//! top-level + nested type/method declaration, in source byte order.
//! `Symbol::sibling_names` stays empty; the language-agnostic chunker
//! populates it later.

use anyhow::Result;
use ohara_core::types::Symbol;

/// Stub. Tasks 2.g–5.g replace this with a real tree-sitter-driven
/// implementation.
pub fn extract(_file_path: &str, _source: &str, _blob_sha: &str) -> Result<Vec<Symbol>> {
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::types::SymbolKind;

    #[test]
    fn extracts_simple_class() {
        let src = "public class Foo { }\n";
        let syms = extract("Foo.java", src, "deadbeef").unwrap();
        assert_eq!(syms.len(), 1, "expected one class symbol, got {syms:?}");
        let s = &syms[0];
        assert_eq!(s.name, "Foo");
        assert_eq!(s.kind, SymbolKind::Class);
        assert_eq!(s.language, "java");
    }

    #[test]
    fn extracts_sealed_interface() {
        // Sealed types appear as ordinary `interface_declaration` /
        // `class_declaration` nodes whose modifiers list contains
        // `sealed`. Capturing the declaration is enough; the modifier
        // ends up inside source_text via the annotation-span work in
        // Task 5.
        let src = "public sealed interface Shape permits Circle, Square { }\n";
        let syms = extract("Shape.java", src, "deadbeef").unwrap();
        assert_eq!(syms.len(), 1, "expected one interface symbol, got {syms:?}");
        let s = &syms[0];
        assert_eq!(s.name, "Shape");
        assert_eq!(s.kind, SymbolKind::Class);
    }
}
