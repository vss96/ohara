use crate::storage::Storage;
use crate::types::{Provenance, RepoId};
use crate::Result;
use async_trait::async_trait;
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

/// Source for "how many commits exist after `since`?" — implemented by the
/// `ohara-git` crate so `ohara-core` stays git-free.
#[async_trait]
pub trait CommitsBehind: Send + Sync {
    async fn count_since(&self, since: Option<&str>) -> Result<u64>;
}

/// Combine the storage-side index status with the git-side commits-behind
/// count to produce a unified `IndexStatus`. Both the CLI `status` command
/// and the MCP `index_status_meta` call this; presentation lives at the call
/// sites.
pub async fn compute_index_status(
    _storage: &dyn Storage,
    _repo_id: &RepoId,
    _behind: &dyn CommitsBehind,
) -> Result<IndexStatus> {
    unimplemented!("compute_index_status — implemented in Step 7")
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

    use crate::storage::{CommitRecord, HunkHit, HunkRecord, Storage};
    use crate::types::{RepoId, Symbol};
    use async_trait::async_trait;

    struct FakeStorage {
        last_sha: Option<String>,
        indexed_at: Option<String>,
    }

    #[async_trait]
    impl Storage for FakeStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: self.last_sha.clone(),
                commits_behind_head: 0,
                indexed_at: self.indexed_at.clone(),
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> { Ok(()) }
        async fn knn_hunks(
            &self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
    }

    struct FakeBehind {
        last_seen_since: std::sync::Mutex<Option<Option<String>>>,
        n: u64,
    }

    #[async_trait]
    impl CommitsBehind for FakeBehind {
        async fn count_since(&self, since: Option<&str>) -> crate::Result<u64> {
            *self.last_seen_since.lock().unwrap() = Some(since.map(str::to_string));
            Ok(self.n)
        }
    }

    #[tokio::test]
    async fn compute_index_status_combines_storage_and_walker() {
        let storage = FakeStorage {
            last_sha: Some("deadbeef".into()),
            indexed_at: Some("2026-01-01T00:00:00Z".into()),
        };
        let behind = FakeBehind {
            last_seen_since: std::sync::Mutex::new(None),
            n: 4,
        };
        let id = RepoId::from_parts("aaaa", "/r");

        let st = compute_index_status(&storage, &id, &behind).await.unwrap();

        assert_eq!(st.last_indexed_commit.as_deref(), Some("deadbeef"));
        assert_eq!(st.commits_behind_head, 4);
        assert_eq!(st.indexed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        // CommitsBehind should be called with the storage watermark.
        let seen = behind.last_seen_since.lock().unwrap().clone();
        assert_eq!(seen, Some(Some("deadbeef".to_string())));
    }
}
