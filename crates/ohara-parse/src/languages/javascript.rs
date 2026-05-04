use anyhow::Result;
use ohara_core::types::Symbol;

const QUERY_SRC: &str = include_str!("../../queries/javascript.scm");

pub fn extract(_file_path: &str, _source: &str, _blob_sha: &str) -> Result<Vec<Symbol>> {
    // Implemented in Phase 2.
    let _ = QUERY_SRC;
    Ok(vec![])
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
