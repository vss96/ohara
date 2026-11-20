//! Plan 20 — BM25-by-text retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::perf_trace::timed_phase;
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

pub struct Bm25TextLane {
    storage: Arc<dyn Storage>,
}

impl Bm25TextLane {
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
        if !profile.is_lane_enabled(LaneId::Bm25Text) {
            return Ok(vec![]);
        }
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        timed_phase(
            "lane_fts_text",
            self.storage.bm25_hunks_by_text(
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
impl RetrievalLane for Bm25TextLane {
    fn id(&self) -> LaneId {
        LaneId::Bm25Text
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

    struct Bm25TextStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::storage::StorageRead for Bm25TextStorage {
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            Ok(self.0.clone())
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

        async fn bm25_hunks_by_semantic_text(
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
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "bbb".into(),
                file_path: "src/lib.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Modified,
                diff_text: "+fn bar() {}".into(),
            },
            commit: CommitMeta {
                commit_sha: "bbb".into(),
                parent_sha: None,
                is_merge: false,
                author: Some("bob".into()),
                ts: 1_700_000_000,
                message: "add bar".into(),
            },
            similarity: 0.7,
        }
    }

    #[tokio::test]
    async fn bm25_text_lane_returns_fts_hits() {
        let hit = make_hit(10);
        let storage: Arc<dyn crate::storage::StorageRead> =
            Arc::new(Bm25TextStorage(vec![hit.clone()]));
        let lane = Bm25TextLane::new(storage);

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
        assert_eq!(hits[0].hunk_id, 10);
    }

    #[tokio::test]
    async fn bm25_text_lane_self_skips_when_text_disabled() {
        use crate::query_understanding::RetrievalProfile;
        let hit = make_hit(11);
        let storage: Arc<dyn crate::storage::StorageRead> = Arc::new(Bm25TextStorage(vec![hit]));
        let lane = Bm25TextLane::new(storage);

        let profile = RetrievalProfile {
            name: "test".into(),
            recency_multiplier: 1.0,
            vec_lane_enabled: true,
            text_lane_enabled: false, // disabled
            symbol_lane_enabled: true,
            rerank_top_k: None,
            explanation: "test".into(),
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
