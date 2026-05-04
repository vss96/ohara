use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
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
        let mut func_name: Option<String> = None;
        let mut func_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = query.capture_names()[cap.index as usize];
            let n = cap.node;
            match cap_name {
                "func_name" => {
                    func_name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "def_function" => {
                    func_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(name), Some((s, e))) = (func_name, func_range) {
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "javascript".to_string(),
                kind: SymbolKind::Function,
                name,
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
}
