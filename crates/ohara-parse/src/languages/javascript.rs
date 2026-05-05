use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use std::collections::HashMap;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

const QUERY_SRC: &str = include_str!("../../queries/javascript.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
    parser
        .set_language(&language)
        .context("set javascript language")?;
    let tree = parser.parse(source, None).context("parse javascript")?;
    let query = Query::new(&language, QUERY_SRC).context("javascript query")?;
    let mut cursor = QueryCursor::new();

    let mut out: Vec<Symbol> = Vec::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut class_name: Option<String> = None;
        let mut class_range: Option<(usize, usize)> = None;
        let mut method_name: Option<String> = None;
        let mut method_range: Option<(usize, usize)> = None;
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;
        let mut arrow_name: Option<String> = None;
        let mut arrow_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => func_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "method_name" => method_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "class_name" => class_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "arrow_name" => arrow_name = Some(n.utf8_text(source.as_bytes())?.to_string()),
                "def_function" => func_range = Some((n.start_byte(), n.end_byte())),
                "def_method" => method_range = Some((n.start_byte(), n.end_byte())),
                "def_class" => class_range = Some((n.start_byte(), n.end_byte())),
                "def_arrow" => arrow_range = Some((n.start_byte(), n.end_byte())),
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (class_name, class_range) {
            out.push(make_symbol(
                file_path,
                blob_sha,
                SymbolKind::Class,
                name,
                s,
                e,
                source,
            ));
        }
        if let (Some(name), Some((s, e))) = (method_name, method_range) {
            out.push(make_symbol(
                file_path,
                blob_sha,
                SymbolKind::Method,
                name,
                s,
                e,
                source,
            ));
        }
        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(make_symbol(
                file_path,
                blob_sha,
                SymbolKind::Function,
                name,
                s,
                e,
                source,
            ));
        }
        if let (Some(name), Some((s, e))) = (arrow_name, arrow_range) {
            out.push(make_symbol(
                file_path,
                blob_sha,
                SymbolKind::Function,
                name,
                s,
                e,
                source,
            ));
        }
    }

    // Dedupe by (span_start, span_end). When the same span is captured by
    // multiple patterns, prefer Method/Class over Function.
    let mut by_span: HashMap<(u32, u32), Symbol> = HashMap::new();
    for sym in out {
        let key = (sym.span_start, sym.span_end);
        match by_span.get(&key) {
            None => {
                by_span.insert(key, sym);
            }
            Some(existing) => {
                if existing.kind == SymbolKind::Function
                    && (sym.kind == SymbolKind::Method || sym.kind == SymbolKind::Class)
                {
                    by_span.insert(key, sym);
                }
            }
        }
    }
    Ok(by_span.into_values().collect())
}

fn make_symbol(
    file_path: &str,
    blob_sha: &str,
    kind: SymbolKind,
    name: String,
    s: usize,
    e: usize,
    source: &str,
) -> Symbol {
    Symbol {
        file_path: file_path.to_string(),
        language: "javascript".to_string(),
        kind,
        name,
        qualified_name: None,
        sibling_names: Vec::new(),
        span_start: s as u32,
        span_end: e as u32,
        blob_sha: blob_sha.to_string(),
        source_text: source[s..e].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_function_declarations() {
        let src = "function alpha() { return 1; }\nfunction beta(x) { return x; }\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha missing: {names:?}");
        assert!(names.contains(&"beta"), "beta missing: {names:?}");
        for s in &syms {
            assert_eq!(s.language, "javascript");
            assert_eq!(s.file_path, "a.js");
            assert_eq!(s.blob_sha, "deadbeef");
        }
    }

    #[test]
    fn extracts_class_and_method_declarations() {
        let src = "class Foo {\n  bar() { return 1; }\n  baz(x) { return x; }\n}\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "Foo class missing: {names:?}");
        assert!(names.contains(&"bar"), "bar method missing: {names:?}");
        assert!(names.contains(&"baz"), "baz method missing: {names:?}");
        let foo = syms.iter().find(|s| s.name == "Foo").unwrap();
        assert!(matches!(foo.kind, ohara_core::types::SymbolKind::Class));
        let bar = syms.iter().find(|s| s.name == "bar").unwrap();
        assert!(matches!(bar.kind, ohara_core::types::SymbolKind::Method));
    }

    #[test]
    fn extracts_class_with_no_methods() {
        // An empty class and a class with only field declarations both
        // need to produce a Class symbol — earlier the class capture was
        // nested inside the method-definition pattern, so classes
        // without method bodies were silently dropped.
        let src = "class Empty {}\n\
                   class WithFields {\n  x;\n  y = 0;\n}\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Empty"), "Empty class missing: {names:?}");
        assert!(
            names.contains(&"WithFields"),
            "WithFields class missing: {names:?}"
        );
        let empty = syms.iter().find(|s| s.name == "Empty").unwrap();
        assert!(matches!(empty.kind, ohara_core::types::SymbolKind::Class));
        let with_fields = syms.iter().find(|s| s.name == "WithFields").unwrap();
        assert!(matches!(
            with_fields.kind,
            ohara_core::types::SymbolKind::Class
        ));
    }

    #[test]
    fn extracts_arrow_function_const() {
        let src = "const handle = (req, res) => { return res.json({}); };\n\
                   export const greet = name => `hi ${name}`;\n";
        let syms = extract("a.js", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"handle"), "handle missing: {names:?}");
        assert!(names.contains(&"greet"), "greet missing: {names:?}");
        let handle = syms.iter().find(|s| s.name == "handle").unwrap();
        assert!(matches!(
            handle.kind,
            ohara_core::types::SymbolKind::Function
        ));
    }
}
