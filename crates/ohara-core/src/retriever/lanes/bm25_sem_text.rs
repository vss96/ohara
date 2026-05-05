//! Plan 25 — BM25-by-semantic-text retrieval lane.
//!
//! This is the 4th BM25 lane: it queries `bm25_hunks_by_semantic_text`
//! (FTS5 over `hunk.semantic_text`, the contextual preamble + added-
//! lines blob built at index time by `hunk_text::build`). It runs in
//! parallel with the existing `bm25_text` (raw `diff_text`),
//! `bm25_hist_sym`, and `bm25_head_sym` lanes; its hits fuse into the
//! same RRF call so no separate ranking knob is needed.

use super::{LaneId, RetrievalLane};
use crate::perf_trace::timed_phase;
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

pub struct Bm25SemTextLane {
    storage: Arc<dyn Storage>,
}

impl Bm25SemTextLane {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Bm25SemText) {
            return Ok(vec![]);
        }
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        timed_phase(
            "lane_fts_semantic",
            self.storage.bm25_hunks_by_semantic_text(
                repo_id,
                &query.query,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            ),
        )
        .await
    }
}

#[async_trait]
impl RetrievalLane for Bm25SemTextLane {
    fn id(&self) -> LaneId {
        LaneId::Bm25SemText
    }

    async fn search(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
    ) -> crate::Result<Vec<HunkHit>> {
        let profile = crate::query_understanding::RetrievalProfile::for_intent(
            crate::query_understanding::parse_query(&query.query).intent,
        );
        self.search_with_profile(query, repo_id, k, &profile).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::PatternQuery;
    use crate::storage::{HunkHit, HunkId};
    use crate::types::RepoId;
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Minimal storage that returns scripted hits from
    /// `bm25_hunks_by_semantic_text` and unreachable from every other
    /// method.
    struct SemStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for SemStorage {
        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
        }
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> {
            Ok(crate::query::IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn put_commit(
            &self,
            _: &RepoId,
            _: &crate::storage::CommitRecord,
        ) -> crate::Result<()> {
            Ok(())
        }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
            Ok(false)
        }
        async fn put_hunks(
            &self,
            _: &RepoId,
            _: &[crate::storage::HunkRecord],
        ) -> crate::Result<()> {
            Ok(())
        }
        async fn put_head_symbols(
            &self,
            _: &RepoId,
            _: &[crate::types::Symbol],
        ) -> crate::Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> {
            Ok(())
        }
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(vec![])
        }
        async fn get_hunk_symbols(
            &self,
            _: &RepoId,
            _: HunkId,
        ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
            Ok(vec![])
        }
        async fn get_hunk_symbols_batch(
            &self,
            _: &RepoId,
            _: &[HunkId],
        ) -> crate::Result<std::collections::HashMap<HunkId, Vec<crate::types::HunkSymbol>>>
        {
            Ok(std::collections::HashMap::new())
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn get_commit(
            &self,
            _: &RepoId,
            _: &str,
        ) -> crate::Result<Option<crate::types::CommitMeta>> {
            Ok(None)
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
        ) -> crate::Result<Vec<crate::types::Hunk>> {
            Ok(vec![])
        }
        async fn get_neighboring_file_commits(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
            _: u8,
            _: u8,
        ) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> {
            Ok(vec![])
        }
        async fn get_index_metadata(
            &self,
            _: &RepoId,
        ) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
            Ok(crate::index_metadata::StoredIndexMetadata::default())
        }
        async fn put_index_metadata(
            &self,
            _: &RepoId,
            _: &[(String, String)],
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "ccc".into(),
                file_path: "src/lib.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Modified,
                diff_text: "+fn baz() {}".into(),
            },
            commit: CommitMeta {
                commit_sha: "ccc".into(),
                parent_sha: None,
                is_merge: false,
                author: Some("c".into()),
                ts: 1_700_000_000,
                message: "add baz".into(),
            },
            similarity: 0.6,
        }
    }

    #[tokio::test]
    async fn bm25_sem_text_lane_returns_fts_hits() {
        let hit = make_hit(20);
        let storage: Arc<dyn crate::Storage> = Arc::new(SemStorage(vec![hit.clone()]));
        let lane = Bm25SemTextLane::new(storage);

        let q = PatternQuery {
            query: "retry backoff".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 20);
    }

    #[tokio::test]
    async fn bm25_sem_text_lane_self_skips_when_disabled() {
        use crate::query_understanding::RetrievalProfile;
        let hit = make_hit(21);
        let storage: Arc<dyn crate::Storage> = Arc::new(SemStorage(vec![hit]));
        let lane = Bm25SemTextLane::new(storage);

        let profile = RetrievalProfile {
            semantic_text_lane_enabled: false,
            ..RetrievalProfile::default_unknown()
        };
        let q = PatternQuery {
            query: "retry".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane
            .search_with_profile(&q, &repo_id, 10, &profile)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
