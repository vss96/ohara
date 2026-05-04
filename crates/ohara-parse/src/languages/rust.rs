use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

const QUERY_SRC: &str = include_str!("../../queries/rust.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    parser
        .set_language(&language)
        .context("set rust language")?;
    let tree = parser.parse(source, None).context("parse rust")?;
    let query = Query::new(&language, QUERY_SRC).context("rust query")?;
    let mut cursor = QueryCursor::new();

    let mut out = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut name: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut node_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "name" | "func_name" | "method_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "struct_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "enum_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "def_function" => {
                    kind.get_or_insert(SymbolKind::Function);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_method" => {
                    kind = Some(SymbolKind::Method);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_struct" | "def_enum" => {
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(n), Some(k), Some((s, e))) = (name, kind, node_range) {
            let text = &source[s..e];
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "rust".to_string(),
                kind: k,
                name: n,
                qualified_name: None,
                sibling_names: Vec::new(),
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: text.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_returns_functions_methods_structs_and_enums_with_method_kind() {
        let src = r#"
            fn alpha() {}
            struct Foo;
            impl Foo {
                fn beta(&self) {}
            }
            enum Color { Red }
        "#;
        let syms = extract("a.rs", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Color"));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Method));
    }
}
