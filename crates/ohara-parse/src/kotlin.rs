//! Kotlin symbol extractor (Plan 4).
//!
//! Walks a tree-sitter-kotlin parse tree and emits one `Symbol` per
//! top-level + nested type/function declaration, in source byte order.
//! `Symbol::sibling_names` stays empty; the language-agnostic chunker
//! populates it later.

use anyhow::Result;
use ohara_core::types::Symbol;

/// Stub. Tasks 8.g–10.g replace this with a real tree-sitter-driven
/// implementation.
pub fn extract(_file_path: &str, _source: &str, _blob_sha: &str) -> Result<Vec<Symbol>> {
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::types::SymbolKind;

    #[test]
    fn extracts_data_class() {
        // Kotlin's `data class` is an ordinary `class_declaration`
        // carrying a `data` modifier; per spec it collapses to Class.
        let src = "data class User(val id: Int, val name: String)\n";
        let syms = extract("User.kt", src, "deadbeef").unwrap();
        assert_eq!(syms.len(), 1, "expected one class symbol, got {syms:?}");
        let s = &syms[0];
        assert_eq!(s.name, "User");
        assert_eq!(s.kind, SymbolKind::Class);
        assert_eq!(s.language, "kotlin");
    }

    #[test]
    fn extracts_sealed_class_kt() {
        // Sealed classes (and sealed interfaces) appear as
        // class_declaration with a `sealed` modifier — same node type.
        let src = "sealed class Shape\n";
        let syms = extract("Shape.kt", src, "deadbeef").unwrap();
        assert_eq!(syms.len(), 1, "expected one class symbol, got {syms:?}");
        assert_eq!(syms[0].name, "Shape");
        assert_eq!(syms[0].kind, SymbolKind::Class);
    }

    #[test]
    fn extracts_object_as_class() {
        // `object Foo { ... }` is a singleton in Kotlin; per spec it
        // collapses to SymbolKind::Class (no dedicated Object kind in
        // v0.4).
        let src = "object Singleton { fun ping() {} }\n";
        let syms = extract("Singleton.kt", src, "deadbeef").unwrap();
        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class)
            .expect("no class extracted");
        assert_eq!(class.name, "Singleton");
    }

    #[test]
    fn extracts_companion_object_as_class() {
        // companion objects are nested inside a class. We expect two
        // Class symbols: the outer Foo and the inner Companion.
        // Kotlin allows omitting the companion's name, in which case
        // the symbol's name is "Companion" (the implicit identifier).
        let src = "\
class Foo {
    companion object Helper {
        fun create() {}
    }
}
";
        let syms = extract("Foo.kt", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"Foo"), "missing outer class: {names:?}");
        assert!(
            names.contains(&"Helper"),
            "missing companion object: {names:?}"
        );
    }
}
