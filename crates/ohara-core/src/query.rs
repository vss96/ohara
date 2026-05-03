use crate::storage::{HunkId, Storage};
use crate::types::{Provenance, RepoId};
use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternQuery {
    pub query: String,
    pub k: u8,
    pub language: Option<String>,
    pub since_unix: Option<i64>,
    /// Skip the cross-encoder rerank stage even if the Retriever has a
    /// reranker attached. Returns post-RRF ordering with the recency
    /// multiplier still applied. Used by MCP's `no_rerank` flag for
    /// callers that want fast, deterministic results.
    #[serde(default)]
    pub no_rerank: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternHit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    pub commit_date: String, // ISO 8601
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
    pub indexed_at: Option<String>, // ISO 8601
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    pub index_status: IndexStatus,
    pub hint: Option<String>,
    /// Plan 13: index compatibility verdict. `None` for callers that
    /// haven't wired the assessment yet (back-compat with v0.6 MCP
    /// clients that don't know about the field). When present, MCP
    /// clients can surface the reason / next-step command without
    /// re-deriving it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<crate::index_metadata::CompatibilityStatus>,
}

/// Reciprocal Rank Fusion. Each ranking is best-first.
///
/// Score for hunk `h` = sum over lanes of `1.0 / (k as f64 + rank_in_lane(h))`,
/// where `rank_in_lane` is 1-based. Hunks absent from a lane contribute 0
/// from that lane. Returns hunk ids ordered best-first; ties are broken by
/// first-appearance across the input rankings.
///
/// `k` is the RRF smoothing constant (Cormack et al. recommend 60).
pub fn reciprocal_rank_fusion(rankings: &[Vec<HunkId>], k: u32) -> Vec<HunkId> {
    let k_f = k as f64;
    let mut scores: HashMap<HunkId, f64> = HashMap::new();
    let mut first_seen: HashMap<HunkId, usize> = HashMap::new();
    let mut order_counter: usize = 0;
    for ranking in rankings {
        for (i, &id) in ranking.iter().enumerate() {
            // 1-based rank is the standard RRF convention (Cormack 2009).
            let rank = (i + 1) as f64;
            *scores.entry(id).or_insert(0.0) += 1.0 / (k_f + rank);
            first_seen.entry(id).or_insert_with(|| {
                let n = order_counter;
                order_counter += 1;
                n
            });
        }
    }
    // `first_seen` is populated above for every id we score, so the
    // `unwrap_or(usize::MAX)` is a defensive belt-and-suspenders fallback
    // — the MAX sentinel just sinks any unexpected miss to the bottom of
    // the tie-break order without panicking.
    let mut entries: Vec<(HunkId, f64, usize)> = scores
        .into_iter()
        .map(|(id, s)| (id, s, *first_seen.get(&id).unwrap_or(&usize::MAX)))
        .collect();
    // Sort by score descending; tie-break by first-appearance ascending so
    // the result is deterministic for callers.
    entries.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });
    entries.into_iter().map(|(id, _, _)| id).collect()
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
    storage: &dyn Storage,
    repo_id: &RepoId,
    behind: &dyn CommitsBehind,
) -> Result<IndexStatus> {
    let st = storage.get_index_status(repo_id).await?;
    let n = behind
        .count_since(st.last_indexed_commit.as_deref())
        .await?;
    Ok(IndexStatus {
        last_indexed_commit: st.last_indexed_commit,
        commits_behind_head: n,
        indexed_at: st.indexed_at,
    })
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
            compatibility: None,
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
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: self.last_sha.clone(),
                commits_behind_head: 0,
                indexed_at: self.indexed_at.clone(),
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
            unreachable!("compute_index_status should not exercise commit_exists")
        }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> {
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> {
            unreachable!("compute_index_status should not exercise clear_head_symbols")
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
            unreachable!("compute_index_status should not exercise bm25_hunks_by_text")
        }
        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            unreachable!("compute_index_status should not exercise bm25_hunks_by_semantic_text")
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            unreachable!("compute_index_status should not exercise bm25_hunks_by_symbol_name")
        }
        async fn bm25_hunks_by_historical_symbol(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            unreachable!("compute_index_status should not exercise bm25_hunks_by_historical_symbol")
        }
        async fn get_hunk_symbols(
            &self,
            _: &RepoId,
            _: crate::storage::HunkId,
        ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
            unreachable!("compute_index_status should not exercise get_hunk_symbols")
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
            unreachable!("compute_index_status should not exercise get_commit")
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
        ) -> crate::Result<Vec<crate::types::Hunk>> {
            unreachable!("compute_index_status should not exercise get_hunks_for_file_in_commit")
        }
        async fn get_neighboring_file_commits(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
            _: u8,
            _: u8,
        ) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> {
            unreachable!("compute_index_status should not exercise get_neighboring_file_commits")
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

    // ----- Reciprocal Rank Fusion tests (Plan 3 / Track D) ----------------

    #[test]
    fn rrf_combines_three_lanes_with_default_k() {
        // Each id appears in every lane; lane orders permute the ids so we
        // verify the function aggregates across lanes rather than returning
        // a single lane verbatim. Length must be 3, all three ids present.
        let lane1: Vec<HunkId> = vec![1, 2, 3];
        let lane2: Vec<HunkId> = vec![2, 3, 1];
        let lane3: Vec<HunkId> = vec![3, 1, 2];
        let out = reciprocal_rank_fusion(&[lane1, lane2, lane3], 60);
        assert_eq!(out.len(), 3, "fused output must contain every unique id");
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3]);
    }

    #[test]
    fn rrf_handles_disjoint_lanes() {
        let lane1: Vec<HunkId> = vec![10, 20];
        let lane2: Vec<HunkId> = vec![30, 40];
        let lane3: Vec<HunkId> = vec![];
        let out = reciprocal_rank_fusion(&[lane1, lane2, lane3], 60);
        assert_eq!(out.len(), 4, "disjoint lanes must union all ids");
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(sorted, vec![10, 20, 30, 40]);
    }

    #[test]
    fn rrf_empty_input_returns_empty() {
        let out = reciprocal_rank_fusion(&[], 60);
        assert!(out.is_empty());
    }

    #[test]
    fn rrf_two_lane_hand_computed_example() {
        // lane1 = [a=1, b=2, c=3], lane2 = [c=3, a=1, b=2], k = 60.
        // Hand-computed scores (1-based ranks):
        //   a: 1/61 + 1/62 ≈ 0.032525
        //   b: 1/62 + 1/63 ≈ 0.031999
        //   c: 1/63 + 1/61 ≈ 0.032273
        // Order: a > c > b.
        let lane1: Vec<HunkId> = vec![1, 2, 3];
        let lane2: Vec<HunkId> = vec![3, 1, 2];
        let out = reciprocal_rank_fusion(&[lane1, lane2], 60);
        assert_eq!(out, vec![1, 3, 2], "RRF order must match hand-computation");
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
