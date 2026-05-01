use serde::{Deserialize, Serialize};

/// Stable identifier for a repository on a single machine.
///
/// Hash of `first_commit_sha` + canonical absolute path. Stable across
/// renames within the same path, unique across multiple clones.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoId(String);

impl RepoId {
    pub fn from_parts(first_commit_sha: &str, canonical_path: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(first_commit_sha.as_bytes());
        h.update(b"\0");
        h.update(canonical_path.as_bytes());
        Self(hex::encode(&h.finalize()[..16]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_deterministic() {
        let a = RepoId::from_parts("deadbeef", "/Users/x/projects/foo");
        let b = RepoId::from_parts("deadbeef", "/Users/x/projects/foo");
        assert_eq!(a, b);
    }

    #[test]
    fn repo_id_distinguishes_clones_by_path() {
        let a = RepoId::from_parts("deadbeef", "/Users/x/foo");
        let b = RepoId::from_parts("deadbeef", "/Users/x/foo-2");
        assert_ne!(a, b);
    }

    #[test]
    fn repo_id_distinguishes_repos_by_first_commit() {
        let a = RepoId::from_parts("aaaa", "/Users/x/foo");
        let b = RepoId::from_parts("bbbb", "/Users/x/foo");
        assert_ne!(a, b);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Provenance {
    /// Pulled from a parsed AST or another deterministic source. Used by
    /// the symbol extractor today.
    Extracted,
    /// Returned by a similarity-ranked retriever — semantic match, not
    /// git-truth. Used by `find_pattern`.
    Inferred,
    /// Sourced directly from `git blame` — every reported line is
    /// attributable to the named commit. Used by `explain_change`
    /// (Plan 5). Distinct from `Extracted` so callers can distinguish
    /// "AST said so" from "git said so".
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Const,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMeta {
    pub commit_sha: String,
    pub parent_sha: Option<String>,
    pub is_merge: bool,
    pub author: Option<String>,
    pub ts: i64, // unix seconds
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub commit_sha: String,
    pub file_path: String,
    pub language: Option<String>,
    pub change_kind: ChangeKind,
    pub diff_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file_path: String,
    pub language: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: Option<String>,
    /// Names of sibling AST nodes merged into the same chunk by the
    /// AST-aware sibling-merge chunker. Empty for v0.2-era rows or for
    /// chunks containing a single top-level symbol.
    #[serde(default)]
    pub sibling_names: Vec<String>,
    pub span_start: u32,
    pub span_end: u32,
    pub blob_sha: String,
    pub source_text: String,
}

#[cfg(test)]
mod symbol_tests {
    use super::*;

    #[test]
    fn symbol_sibling_names_round_trip() {
        // Track C / step C-1: `Symbol` round-trips a sibling_names field
        // through serde. Construct with two siblings, serialize, inspect
        // the raw JSON, then deserialize back into a typed Symbol.
        let s = Symbol {
            file_path: "src/a.rs".into(),
            language: "rust".into(),
            kind: SymbolKind::Function,
            name: "alpha".into(),
            qualified_name: None,
            sibling_names: vec!["beta".into(), "gamma".into()],
            span_start: 0,
            span_end: 42,
            blob_sha: "deadbeef".into(),
            source_text: "fn alpha() {}".into(),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let names = v
            .get("sibling_names")
            .expect("`Symbol` must serialize a `sibling_names` field");
        let arr = names.as_array().expect("`sibling_names` must be an array");
        assert_eq!(
            arr.iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec!["beta".to_string(), "gamma".to_string()]
        );

        let back: Symbol = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.sibling_names,
            vec!["beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(back.name, "alpha");
        assert_eq!(back.span_end, 42);
    }
}
