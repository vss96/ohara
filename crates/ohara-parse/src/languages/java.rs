//! Java symbol extractor.
//!
//! Walks a tree-sitter-java parse tree and emits one `Symbol` per
//! top-level + nested type/method declaration, in source byte order.
//! `Symbol::sibling_names` stays empty; the language-agnostic chunker
//! populates it later.

use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

const QUERY_SRC: &str = include_str!("../../queries/java.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    parser
        .set_language(&language)
        .context("set java language")?;
    let tree = parser.parse(source, None).context("parse java")?;
    let query = Query::new(&language, QUERY_SRC).context("java query")?;
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
                "class_name" | "method_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "def_class" => {
                    kind = Some(SymbolKind::Class);
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
                language: "java".to_string(),
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

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn extracts_methods_inside_class() {
        let src = "\
public class Calc {
    public int add(int a, int b) { return a + b; }
    public int sub(int a, int b) { return a - b; }
}
";
        let syms = extract("Calc.java", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Calc"), "missing class: {names:?}");
        assert!(names.contains(&"add"), "missing add: {names:?}");
        assert!(names.contains(&"sub"), "missing sub: {names:?}");
        let methods: Vec<&Symbol> = syms
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert_eq!(
            methods.len(),
            2,
            "expected two Method symbols, got {methods:?}"
        );
    }

    #[test]
    fn preserves_annotations_in_source_text() {
        // Spring-friendly span behavior: source_text must include the
        // preceding @RestController / @RequestMapping annotations so
        // embedding + BM25 indexing pick them up. The same convention
        // applies to annotated methods.
        //
        // tree-sitter-java already absorbs preceding annotations and
        // modifiers into the `modifiers` child of class_declaration /
        // method_declaration / record_declaration / etc., so the
        // declaration node's byte range naturally starts at the first
        // annotation. No span-extension code is needed; this test is
        // a regression guard against grammar changes that might split
        // annotations into a sibling node.
        let src = "\
@RestController
@RequestMapping(\"/users\")
public class UserController {
    @GetMapping(\"/{id}\")
    public User get(@PathVariable Long id) { return null; }
}
";
        let syms = extract("UserController.java", src, "deadbeef").unwrap();

        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class && s.name == "UserController")
            .expect("controller class missing");
        assert!(
            class.source_text.starts_with("@RestController"),
            "class source_text should start with @RestController, got: {:?}",
            &class.source_text[..class.source_text.len().min(80)]
        );
        assert!(
            class.source_text.contains("@RequestMapping(\"/users\")"),
            "class source_text should contain @RequestMapping line"
        );

        let method = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Method && s.name == "get")
            .expect("get method missing");
        assert!(
            method.source_text.starts_with("@GetMapping"),
            "method source_text should start with @GetMapping, got: {:?}",
            &method.source_text[..method.source_text.len().min(80)]
        );
    }

    #[test]
    fn extracts_record_as_class() {
        // Java 14+ record. tree-sitter-java models this as a distinct
        // record_declaration AST node, but per spec it collapses to
        // SymbolKind::Class — there's no Record kind in v0.4.
        let src = "public record Point(int x, int y) { }\n";
        let syms = extract("Point.java", src, "deadbeef").unwrap();
        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class)
            .expect("no class extracted");
        assert_eq!(class.name, "Point");
    }

    #[test]
    fn extracts_enum() {
        let src = "public enum Color { RED, GREEN, BLUE }\n";
        let syms = extract("Color.java", src, "deadbeef").unwrap();
        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class)
            .expect("no class extracted");
        assert_eq!(class.name, "Color");
    }

    #[test]
    fn extracts_annotation_type() {
        // `@interface Auditable` is an annotation type. Maps to Class.
        let src = "public @interface Auditable { String value() default \"\"; }\n";
        let syms = extract("Auditable.java", src, "deadbeef").unwrap();
        let class = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Class && s.name == "Auditable")
            .expect("no annotation-type class extracted");
        assert_eq!(class.name, "Auditable");
    }

    #[test]
    fn constructor_kind_is_method() {
        // Java spec quirk: constructors share SymbolKind::Method but
        // their `name` is the enclosing class's identifier, not the
        // string "<init>" or anything synthetic.
        let src = "\
public class Box {
    public Box(int n) { }
}
";
        let syms = extract("Box.java", src, "deadbeef").unwrap();
        let ctor = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Method)
            .expect("no constructor extracted");
        assert_eq!(ctor.name, "Box", "constructor name should be class name");
    }
}
