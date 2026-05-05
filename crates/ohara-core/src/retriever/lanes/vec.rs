//! Plan 20 — vector-KNN retrieval lane.

use super::{LaneId, RetrievalLane};
use crate::embed::EmbeddingProvider;
use crate::perf_trace::timed_phase;
use crate::query::PatternQuery;
use crate::query_understanding::RetrievalProfile;
use crate::storage::{HunkHit, Storage};
use crate::types::RepoId;
use async_trait::async_trait;
use std::sync::Arc;

/// Retrieval lane: vector KNN on hunk embeddings.
///
/// Embeds the query text once per `search` call using the injected
/// `EmbeddingProvider`. The embed step is inside the lane so the
/// coordinator does not need to know which lanes require embeddings.
pub struct VecLane {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn EmbeddingProvider>,
}

impl VecLane {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self { storage, embedder }
    }

    /// Profile-parameterised search, used in unit tests to inject
    /// an explicit `RetrievalProfile` without going through `parse_query`.
    pub async fn search_with_profile(
        &self,
        query: &PatternQuery,
        repo_id: &RepoId,
        k: usize,
        profile: &RetrievalProfile,
    ) -> crate::Result<Vec<HunkHit>> {
        if !profile.is_lane_enabled(LaneId::Vec) {
            return Ok(vec![]);
        }
        let q_text = vec![query.query.clone()];
        let mut embs = timed_phase("embed_query", self.embedder.embed_batch(&q_text)).await?;
        let q_emb = embs
            .pop()
            .ok_or_else(|| crate::OhraError::Embedding("embed_batch returned empty".into()))?;
        let since_unix = query
            .since_unix
            .or_else(|| crate::query_understanding::parse_query(&query.query).since_unix);
        timed_phase(
            "lane_knn",
            self.storage.knn_hunks(
                repo_id,
                &q_emb,
                u8::try_from(k).unwrap_or(u8::MAX),
                query.language.as_deref(),
                since_unix,
            ),
        )
        .await
    }
}

#[async_trait]
impl RetrievalLane for VecLane {
    fn id(&self) -> LaneId {
        LaneId::Vec
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

    // Minimal fake storage: returns a preset hit list for knn_hunks.
    struct KnnStorage(Vec<HunkHit>);

    #[async_trait]
    impl crate::Storage for KnnStorage {
        // Only knn_hunks needs an actual implementation.
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
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

    struct ZeroEmbedder;
    #[async_trait]
    impl crate::EmbeddingProvider for ZeroEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "zero"
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }

    fn make_hit(id: HunkId) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: "abc".into(),
                file_path: "src/lib.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn foo() {}".into(),
            },
            commit: CommitMeta {
                commit_sha: "abc".into(),
                parent_sha: None,
                is_merge: false,
                author: Some("alice".into()),
                ts: 1_700_000_000,
                message: "add foo".into(),
            },
            similarity: 0.9,
        }
    }

    #[tokio::test]
    async fn vec_lane_returns_knn_hits() {
        let hit = make_hit(1);
        let storage: Arc<dyn crate::Storage> = Arc::new(KnnStorage(vec![hit.clone()]));
        let embedder: Arc<dyn crate::EmbeddingProvider> = Arc::new(ZeroEmbedder);
        let lane = VecLane::new(storage, embedder);

        let q = PatternQuery {
            query: "retry".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane.search(&q, &repo_id, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hunk_id, 1);
    }

    #[tokio::test]
    async fn vec_lane_self_skips_when_profile_disables_it() {
        use crate::query_understanding::RetrievalProfile;

        let hit = make_hit(2);
        let storage: Arc<dyn crate::Storage> = Arc::new(KnnStorage(vec![hit]));
        let embedder: Arc<dyn crate::EmbeddingProvider> = Arc::new(ZeroEmbedder);
        let lane = VecLane::new(storage, embedder);

        let q = PatternQuery {
            query: "config env".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let profile = RetrievalProfile {
            name: "test".into(),
            recency_multiplier: 1.0,
            vec_lane_enabled: false, // <-- disabled
            text_lane_enabled: true,
            symbol_lane_enabled: true,
            rerank_top_k: None,
            explanation: "test".into(),
            ..RetrievalProfile::default_unknown()
        };
        let repo_id = RepoId::from_parts("sha", "/repo");
        let hits = lane
            .search_with_profile(&q, &repo_id, 10, &profile)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "vec lane must self-skip when vec_lane_enabled=false"
        );
    }
}
