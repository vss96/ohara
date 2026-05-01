use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use std::collections::HashMap;
use tree_sitter::{Parser, Query, QueryCursor};

const QUERY_SRC: &str = include_str!("../queries/python.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    let language = tree_sitter_python::language();
    parser
        .set_language(&language)
        .context("set python language")?;
    let tree = parser.parse(source, None).context("parse python")?;
    let query = Query::new(&language, QUERY_SRC).context("python query")?;
    let mut cursor = QueryCursor::new();

    let mut out: Vec<Symbol> = Vec::new();

    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        // Each match may carry up to two distinct symbols: a class definition
        // and (if the pattern matches a class containing a method) the method
        // itself. Track them independently so a single match can emit both.
        let mut class_name: Option<String> = None;
        let mut class_range: Option<(usize, usize)> = None;
        let mut method_name: Option<String> = None;
        let mut method_range: Option<(usize, usize)> = None;
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => {
                    func_name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "method_name" => {
                    method_name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "class_name" => {
                    class_name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "def_function" => {
                    func_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_method" => {
                    method_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_class" => {
                    class_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (class_name, class_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "python".to_string(),
                kind: SymbolKind::Class,
                name,
                qualified_name: None,
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: source[s..e].to_string(),
            });
        }
        if let (Some(name), Some((s, e))) = (method_name, method_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "python".to_string(),
                kind: SymbolKind::Method,
                name,
                qualified_name: None,
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: source[s..e].to_string(),
            });
        }
        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "python".to_string(),
                kind: SymbolKind::Function,
                name,
                qualified_name: None,
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: source[s..e].to_string(),
            });
        }
    }

    // Dedupe by (span_start, span_end). Prefer Method/Class over Function when
    // the same span is captured by multiple patterns (e.g. a method inside a
    // class is also matched by the top-level `function_definition` pattern).
    // Dedup key is (span_start, span_end) only — safe because all symbols here
    // share the same file_path (we're inside a single `extract` call).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_functions_and_class_methods() {
        let src = "def alpha():\n    pass\nclass Foo:\n    def beta(self):\n        pass\n";
        let syms = extract("a.py", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"beta"));
    }
}
