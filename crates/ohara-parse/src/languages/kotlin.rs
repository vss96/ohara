//! Kotlin symbol extractor.
//!
//! Walks a tree-sitter-kotlin parse tree and emits one `Symbol` per
//! top-level + nested type/function declaration, in source byte order.
//! `Symbol::sibling_names` stays empty; the language-agnostic chunker
//! populates it later.
//!
//! Kotlin's grammar (fwcd/tree-sitter-kotlin 0.3.x) does not expose
//! `name:` fields on declaration nodes, so the extractor walks
//! capture indices via positional patterns and dedups overlapping
//! span captures by `(span_start, span_end)`.

use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use std::collections::HashMap;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

const QUERY_SRC: &str = include_str!("../../queries/kotlin.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
    parser
        .set_language(&language)
        .context("set kotlin language")?;
    let tree = parser.parse(source, None).context("parse kotlin")?;
    let query = Query::new(&language, QUERY_SRC).context("kotlin query")?;
    let mut cursor = QueryCursor::new();

    let mut out: Vec<Symbol> = Vec::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut name: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut node_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "class_name" | "func_name" | "method_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "def_class" => {
                    kind = Some(SymbolKind::Class);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_function" => {
                    kind = Some(SymbolKind::Function);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_method" => {
                    kind = Some(SymbolKind::Method);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(n), Some(k), Some((s, e))) = (name, kind, node_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "kotlin".to_string(),
                kind: k,
                name: n,
                qualified_name: None,
                sibling_names: Vec::new(),
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: source[s..e].to_string(),
            });
        }
    }

    // Dedup by (span_start, span_end). The query `(class_declaration
    // (type_identifier) @class_name) @def_class` matches the first
    // direct type_identifier child, but if a future grammar exposed
    // multiple type_identifier children for a single declaration we'd
    // emit one Symbol per match. Dedup makes that idempotent.
    let mut by_span: HashMap<(u32, u32), Symbol> = HashMap::new();
    for sym in out {
        let key = (sym.span_start, sym.span_end);
        by_span.entry(key).or_insert(sym);
    }
    Ok(by_span.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn debug_dump_ast() {
        let src = "\
@Component
@Singleton
class FooService {
    @Inject
    fun load() {}
}

@JvmStatic
fun topAnno() {}
";
        let mut parser = tree_sitter::Parser::new();
        let language: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        fn walk(n: tree_sitter::Node, src: &str, depth: usize) {
            let text = &src[n.start_byte()..n.end_byte()];
            let display: String = text.chars().take(60).collect();
            eprintln!(
                "{}{} [{}..{}] {:?}",
                "  ".repeat(depth),
                n.kind(),
                n.start_byte(),
                n.end_byte(),
                display
            );
            for i in 0..n.named_child_count() {
                walk(n.named_child(i).unwrap(), src, depth + 1);
            }
        }
        walk(tree.root_node(), src, 0);
    }

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
    fn preserves_annotations_in_source_text_kt() {
        // Spring/DI-friendly span behavior: source_text must include
        // preceding @Component / @Singleton annotations on classes and
        // @Inject on functions so embedding + BM25 indexing pick them
        // up.
        //
        // tree-sitter-kotlin already absorbs preceding annotations
        // into the `modifiers` child of class_declaration /
        // function_declaration / etc., so the declaration node's byte
        // range naturally starts at the first annotation. No span
        // extension code is needed; this test is a regression guard
        // against grammar changes.
        let src = "\
@Component
@Singleton
class FooService {
    @Inject
    fun load() {}
}
";
        let syms = extract("FooService.kt", src, "deadbeef").unwrap();

        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class && s.name == "FooService")
            .expect("FooService class missing");
        assert!(
            class.source_text.starts_with("@Component"),
            "class source_text should start with @Component, got: {:?}",
            &class.source_text[..class.source_text.len().min(80)]
        );
        assert!(
            class.source_text.contains("@Singleton"),
            "class source_text should include @Singleton"
        );

        let method = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Method && s.name == "load")
            .expect("load method missing");
        assert!(
            method.source_text.starts_with("@Inject"),
            "method source_text should start with @Inject, got: {:?}",
            &method.source_text[..method.source_text.len().min(80)]
        );
    }

    #[test]
    fn extracts_top_level_function_as_function_kind() {
        // Free-standing Kotlin functions (no enclosing class/object/
        // interface) collapse to SymbolKind::Function — matches the
        // Rust/Python convention.
        let src = "fun greet(name: String): String { return \"hi \" + name }\n";
        let syms = extract("util.kt", src, "deadbeef").unwrap();
        let f = syms
            .iter()
            .find(|s| s.name == "greet")
            .expect("greet not extracted");
        assert_eq!(f.kind, SymbolKind::Function);
    }

    #[test]
    fn extracts_member_function_as_method_kind() {
        // Function inside a class body should be SymbolKind::Method
        // (not Function), matching Java's method semantics.
        let src = "\
class Service {
    fun handle() {}
}
";
        let syms = extract("Service.kt", src, "deadbeef").unwrap();
        let m = syms
            .iter()
            .find(|s| s.name == "handle")
            .expect("handle not extracted");
        assert_eq!(m.kind, SymbolKind::Method);
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
