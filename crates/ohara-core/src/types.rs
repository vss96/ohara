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
    Extracted,
    Inferred,
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
    pub sha: String,
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
    pub span_start: u32,
    pub span_end: u32,
    pub blob_sha: String,
    pub source_text: String,
}
