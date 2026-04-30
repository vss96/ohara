use crate::types::Provenance;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternQuery {
    pub query: String,
    pub k: u8,
    pub language: Option<String>,
    pub since_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternHit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    pub commit_date: String,            // ISO 8601
    pub file_path: String,
    pub change_kind: String,
    pub diff_excerpt: String,
    pub diff_truncated: bool,
    pub related_head_symbols: Vec<String>,
    pub similarity: f32,
    pub recency_weight: f32,
    pub combined_score: f32,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    pub last_indexed_commit: Option<String>,
    pub commits_behind_head: u64,
    pub indexed_at: Option<String>,     // ISO 8601
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    pub index_status: IndexStatus,
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Provenance;

    #[test]
    fn pattern_hit_serializes_to_expected_json_shape() {
        let hit = PatternHit {
            commit_sha: "abc".into(),
            commit_message: "msg".into(),
            commit_author: Some("alice".into()),
            commit_date: "2024-01-01T00:00:00Z".into(),
            file_path: "src/foo.rs".into(),
            change_kind: "added".into(),
            diff_excerpt: "+fn x() {}".into(),
            diff_truncated: false,
            related_head_symbols: vec!["foo::x".into()],
            similarity: 0.9,
            recency_weight: 0.5,
            combined_score: 0.78,
            provenance: Provenance::Inferred,
        };
        let s = serde_json::to_string(&hit).unwrap();
        assert!(s.contains("\"provenance\":\"INFERRED\""));
        assert!(s.contains("\"diff_truncated\":false"));
    }

    #[test]
    fn response_meta_round_trips() {
        let meta = ResponseMeta {
            index_status: IndexStatus {
                last_indexed_commit: Some("abc".into()),
                commits_behind_head: 7,
                indexed_at: None,
            },
            hint: None,
        };
        let s = serde_json::to_string(&meta).unwrap();
        let back: ResponseMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.index_status.commits_behind_head, 7);
    }
}
