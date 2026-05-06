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
    /// attributable to the named commit. Used by `explain_change`.
    /// Distinct from `Extracted` so callers can distinguish
    /// "AST said so" from "git said so".
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

impl Default for Hunk {
    fn default() -> Self {
        Self {
            commit_sha: String::new(),
            file_path: String::new(),
            language: None,
            change_kind: ChangeKind::Added,
            diff_text: String::new(),
        }
    }
}

/// Plan 11: confidence for which symbol(s) a hunk touched.
///
/// `ExactSpan` — a changed line intersects a parsed symbol's byte/line
/// span. Highest confidence; the only kind the v0.7 retriever uses to
/// "replace" the file-level symbol lane.
///
/// `HunkHeader` — git's hunk header named an enclosing function/class.
/// Useful when the parser can't reach the file (binary, unsupported
/// language) but git's heuristic still found a context label.
///
/// `FileFallback` — reserved for forward-compat. The v0.7 indexer
/// never writes this kind; storing zero hunk_symbol rows is preferred
/// over pretending file-level attribution is symbol-level. Future
/// plans can opt in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionKind {
    ExactSpan,
    HunkHeader,
    FileFallback,
}

impl AttributionKind {
    /// Stable string used in the `hunk_symbol.attribution_kind`
    /// column. Matches the lowercase variant name.
    pub fn as_str(self) -> &'static str {
        match self {
            AttributionKind::ExactSpan => "exact_span",
            AttributionKind::HunkHeader => "hunk_header",
            AttributionKind::FileFallback => "file_fallback",
        }
    }
}

impl std::str::FromStr for AttributionKind {
    /// Empty error type — callers only need "matched / didn't match"
    /// because the values come from a closed set written by ohara
    /// itself, not arbitrary user input.
    type Err = ();

    /// Parse a stored `attribution_kind` string. Errors when the
    /// value isn't one of the three closed-set values so callers can
    /// decide whether to skip the row or surface a typed error.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "exact_span" => Ok(AttributionKind::ExactSpan),
            "hunk_header" => Ok(AttributionKind::HunkHeader),
            "file_fallback" => Ok(AttributionKind::FileFallback),
            _ => Err(()),
        }
    }
}

/// Plan 11: one symbol a hunk touched, with attribution confidence.
/// The `(commit_sha, file_path)` identity lives on the parent `Hunk`;
/// each `HunkSymbol` carries only the symbol-side payload + kind of
/// attribution evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkSymbol {
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: Option<String>,
    pub attribution: AttributionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file_path: String,
    pub language: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: Option<String>,
    /// Names of sibling AST nodes merged into the same chunk by the
    /// AST-aware sibling-merge chunker. Empty for legacy rows that
    /// pre-date the chunker, or for chunks containing a single
    /// top-level symbol.
    #[serde(default)]
    pub sibling_names: Vec<String>,
    pub span_start: u32,
    pub span_end: u32,
    pub blob_sha: String,
    pub source_text: String,
}

/// Plan 21: opaque newtype wrapping the hex representation of a git blob
/// OID. Used as the file-content key in `BlameCache`.
///
/// Two `ContentHash` values are equal iff they were produced from the same
/// blob OID — which means the file's byte content is identical. The cache
/// provides natural invalidation: a file whose content changes gets a new
/// blob OID, producing a cache miss and a fresh `Blamer::blame_range` call.
///
/// `from_blob_oid` is the only constructor for production callers (where a
/// real `git2::Oid` is available). `from_hex` exists for test and non-git
/// callers that hold a pre-computed hex string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    /// Construct from a git blob OID. The resulting hex string is 40 ASCII
    /// characters (SHA-1) — the length guaranteed by `git2::Oid`.
    pub fn from_blob_oid(oid: git2::Oid) -> Self {
        Self(oid.to_string())
    }

    /// Construct from an already-computed hex string. No validation is
    /// performed — callers are responsible for passing a valid hex OID.
    pub fn from_hex(hex: &str) -> Self {
        Self(hex.to_string())
    }

    /// Construct from arbitrary text (UTF-8). Returns a sha256-hex
    /// string (64 ASCII characters). Used by plan-27's chunk embed
    /// cache to key on the bytes the embedder will consume.
    ///
    /// `from_text` is *distinct* from `from_blob_oid`: that one is
    /// keyed by git's blob hash (40-char SHA-1) for file content;
    /// this one keys cache lookups by the embedder input. Their
    /// outputs share the same `ContentHash` Rust type but live in
    /// different storage tables (`BlameCache` vs `chunk_embed_cache`)
    /// so they cannot collide in practice.
    pub fn from_text(text: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(text.as_bytes());
        Self(hex::encode(digest))
    }

    /// Borrow the underlying hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod content_hash_tests {
    use super::*;

    #[test]
    fn from_hex_round_trips_as_str() {
        // Plan 21 Task A.1: ContentHash constructed from a known hex
        // string must echo it back via as_str() unchanged.
        let h = ContentHash::from_hex("deadbeef1234");
        assert_eq!(h.as_str(), "deadbeef1234");
    }

    #[test]
    fn content_hash_is_eq_and_hash() {
        // Plan 21 Task A.1: ContentHash must be usable as a HashMap key
        // (requires Hash + Eq). Two values built from the same hex string
        // must be equal; two different hex strings must differ.
        use std::collections::HashMap;
        let a = ContentHash::from_hex("aaa");
        let b = ContentHash::from_hex("aaa");
        let c = ContentHash::from_hex("bbb");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut m: HashMap<ContentHash, u8> = HashMap::new();
        m.insert(a.clone(), 1);
        assert_eq!(*m.get(&b).expect("must find by equal key"), 1);
    }

    #[test]
    fn from_blob_oid_produces_40_char_hex() {
        // Plan 21 Task A.1: from_blob_oid wraps git2::Oid::from_str,
        // which produces 40-character hex. Verify the length contract.
        let oid = git2::Oid::from_str("a".repeat(40).as_str()).expect("valid oid");
        let h = ContentHash::from_blob_oid(oid);
        assert_eq!(h.as_str().len(), 40);
    }

    #[test]
    fn from_text_is_deterministic() {
        // Plan 27 Task B.1: same text → same hash.
        let a = ContentHash::from_text("hello world");
        let b = ContentHash::from_text("hello world");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64, "sha256-hex must be 64 chars");
    }

    #[test]
    fn from_text_differs_for_different_inputs() {
        let a = ContentHash::from_text("hello");
        let b = ContentHash::from_text("hellp");
        assert_ne!(a, b);
    }

    #[test]
    fn from_text_empty_input_is_well_defined_and_distinct_from_blob_oid_zero() {
        // Plan 27 Task B.1: from_text("") is the sha256 of the empty
        // string ("e3b0c4..."). It must differ from a from_blob_oid
        // representing all-zeros OID (40 chars, all '0'), which
        // sha256-hex never produces.
        let empty = ContentHash::from_text("");
        assert_eq!(
            empty.as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(
            empty.as_str().len(),
            40,
            "must not collide with a 40-char OID"
        );
    }
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

    #[test]
    fn attribution_kind_round_trips_via_string() {
        // Plan 11 Task 1.2: stored values use the lowercase
        // `exact_span` / `hunk_header` / `file_fallback` form. The
        // round-trip helper backs both the storage layer and the
        // hunk_symbol query helpers.
        use std::str::FromStr;
        for kind in [
            AttributionKind::ExactSpan,
            AttributionKind::HunkHeader,
            AttributionKind::FileFallback,
        ] {
            assert_eq!(AttributionKind::from_str(kind.as_str()), Ok(kind));
        }
        assert_eq!(AttributionKind::from_str("garbage"), Err(()));
    }

    #[test]
    fn hunk_symbol_serialises_with_snake_case_attribution() {
        let hs = HunkSymbol {
            kind: SymbolKind::Function,
            name: "retry_with_backoff".into(),
            qualified_name: Some("net::retry_with_backoff".into()),
            attribution: AttributionKind::ExactSpan,
        };
        let json = serde_json::to_string(&hs).expect("serialize");
        assert!(
            json.contains("\"attribution\":\"exact_span\""),
            "AttributionKind must serialise snake_case: {json}"
        );
        let back: HunkSymbol = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hs);
    }
}
