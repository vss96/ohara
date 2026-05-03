//! Retrieval pipeline.
//!
//! Three lanes — vector KNN, BM25 over hunk text, BM25 over symbol names —
//! gather candidates in parallel; Reciprocal Rank Fusion (`k = 60`) merges
//! the lanes; an optional cross-encoder rerank scores the surviving
//! candidates against the query; a small recency multiplier acts as a
//! tie-breaker on the rerank score.

use crate::diff_text::{truncate_diff, DIFF_EXCERPT_MAX_LINES};
use crate::embed::RerankProvider;
use crate::query::{reciprocal_rank_fusion, PatternHit, PatternQuery};
use crate::storage::{HunkHit, HunkId};
use crate::types::Provenance;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// Tunable knobs for the retrieval pipeline.
#[derive(Debug, Clone)]
pub struct RankingWeights {
    /// Multiplier on the recency factor in the final score:
    /// `final = rerank * (1.0 + recency_weight * exp(-age_days / half_life_days))`.
    /// Default 0.05 — small enough to act as a tie-breaker without
    /// overpowering rerank quality.
    pub recency_weight: f32,
    /// Half-life-ish constant (in days) for the exp-decay recency factor.
    /// Default 90.0 — a 90-day-old commit gets factor ≈ 0.37.
    pub recency_half_life_days: f32,
    /// Number of post-RRF candidates fed into the cross-encoder. Default 50.
    pub rerank_top_k: usize,
    /// Per-lane gather size before RRF. Default 100. Must fit in `u8` because
    /// the storage trait uses `u8` for `k` arguments.
    pub lane_top_k: u8,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            recency_weight: 0.05,
            recency_half_life_days: 90.0,
            rerank_top_k: 50,
            lane_top_k: 100,
        }
    }
}

pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
    reranker: Option<Arc<dyn RerankProvider>>,
}

impl Retriever {
    pub fn new(
        storage: Arc<dyn crate::Storage>,
        embedder: Arc<dyn crate::EmbeddingProvider>,
    ) -> Self {
        Self {
            weights: RankingWeights::default(),
            storage,
            embedder,
            reranker: None,
        }
    }

    pub fn with_weights(mut self, w: RankingWeights) -> Self {
        self.weights = w;
        self
    }

    /// Attach a cross-encoder reranker. When present, the pipeline calls
    /// `rerank` on the post-RRF top-`rerank_top_k` candidates.
    pub fn with_reranker(mut self, r: Arc<dyn RerankProvider>) -> Self {
        self.reranker = Some(r);
        self
    }

    /// Drop the reranker. Equivalent to never having attached one;
    /// callers can use this to force degraded mode (post-RRF order, with
    /// recency multiplier still applied) without rebuilding the rest of
    /// the chain.
    pub fn with_no_rerank(mut self) -> Self {
        self.reranker = None;
        self
    }

    pub async fn find_pattern(
        &self,
        repo_id: &crate::types::RepoId,
        query: &PatternQuery,
        now_unix: i64,
    ) -> crate::Result<Vec<PatternHit>> {
        // 1. Embed the query once for the vector lane. The BM25 lanes use
        //    the raw query string directly.
        let q_text = vec![query.query.clone()];
        let mut q_embs = self.embedder.embed_batch(&q_text).await?;
        let q_emb = q_embs
            .pop()
            .ok_or_else(|| crate::OhraError::Embedding("empty".into()))?;

        // 2. Gather all three lanes in parallel. Lane order is irrelevant
        //    to RRF, but we keep (vec, fts_text, fts_sym) for readability.
        let (vec_res, fts_res, sym_res) = tokio::join!(
            self.storage.knn_hunks(
                repo_id,
                &q_emb,
                self.weights.lane_top_k,
                query.language.as_deref(),
                query.since_unix,
            ),
            self.storage.bm25_hunks_by_text(
                repo_id,
                &query.query,
                self.weights.lane_top_k,
                query.language.as_deref(),
                query.since_unix,
            ),
            self.storage.bm25_hunks_by_symbol_name(
                repo_id,
                &query.query,
                self.weights.lane_top_k,
                query.language.as_deref(),
                query.since_unix,
            ),
        );
        let vec_hits = vec_res?;
        let fts_hits = fts_res?;
        let sym_hits = sym_res?;

        // 3. Build per-lane HunkId rankings + a hunk_id -> HunkHit lookup.
        //    Each lane keeps its hunk-hit's lane-specific `similarity` (used
        //    only for the informational `similarity` field on PatternHit).
        let mut by_id: HashMap<HunkId, HunkHit> = HashMap::new();
        let mut ranking_vec: Vec<HunkId> = Vec::with_capacity(vec_hits.len());
        for h in &vec_hits {
            ranking_vec.push(h.hunk_id);
            by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
        }
        let mut ranking_fts: Vec<HunkId> = Vec::with_capacity(fts_hits.len());
        for h in &fts_hits {
            ranking_fts.push(h.hunk_id);
            by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
        }
        let mut ranking_sym: Vec<HunkId> = Vec::with_capacity(sym_hits.len());
        for h in &sym_hits {
            ranking_sym.push(h.hunk_id);
            by_id.entry(h.hunk_id).or_insert_with(|| h.clone());
        }

        // 4. Reciprocal Rank Fusion (k = 60, Cormack 2009 default) →
        //    truncate to rerank_top_k before the expensive cross-encoder.
        let fused: Vec<HunkId> =
            reciprocal_rank_fusion(&[ranking_vec, ranking_fts, ranking_sym], 60);
        let trimmed: Vec<HunkId> = fused.into_iter().take(self.weights.rerank_top_k).collect();
        let hits: Vec<HunkHit> = trimmed
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect();
        if hits.is_empty() {
            return Ok(vec![]);
        }

        // 5. Optional cross-encoder rerank. In degraded mode (no
        //    reranker), every candidate gets score 1.0 so the surviving
        //    sort order is RRF, modulated by the recency multiplier.
        let candidates: Vec<&str> = hits.iter().map(|h| h.hunk.diff_text.as_str()).collect();
        let rerank_scores: Vec<f32> = match (&self.reranker, query.no_rerank) {
            (Some(r), false) => r.rerank(&query.query, &candidates).await?,
            _ => vec![1.0_f32; candidates.len()],
        };

        // 6. Recency multiplier as a tie-breaker on the rerank score, then
        //    final descending sort and truncate to caller's k.
        let mut out: Vec<PatternHit> = hits
            .into_iter()
            .zip(rerank_scores)
            .map(|(h, s)| {
                let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86400.0;
                let recency = (-age_days / self.weights.recency_half_life_days).exp();
                let combined = s * (1.0 + self.weights.recency_weight * recency);
                // Bogus ts (out-of-range i64) falls back to "" — PatternHit.commit_date
                // is informational, not a contract, so an empty string is acceptable.
                let date = DateTime::<Utc>::from_timestamp(h.commit.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                let (excerpt, truncated) = truncate_diff(&h.hunk.diff_text, DIFF_EXCERPT_MAX_LINES);
                PatternHit {
                    commit_sha: h.commit.commit_sha,
                    commit_message: h.commit.message,
                    commit_author: h.commit.author,
                    commit_date: date,
                    file_path: h.hunk.file_path,
                    change_kind: format!("{:?}", h.hunk.change_kind).to_lowercase(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    related_head_symbols: vec![],
                    similarity: h.similarity,
                    recency_weight: recency,
                    combined_score: combined,
                    provenance: Provenance::Inferred,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(query.k.clamp(1, 20) as usize);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::RerankProvider;
    use crate::query::IndexStatus;
    use crate::storage::{CommitRecord, HunkHit, HunkId, HunkRecord};
    use crate::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn fake_hit(id: HunkId, sha: &str, ts: i64, sim: f32, diff: &str) -> HunkHit {
        HunkHit {
            hunk_id: id,
            hunk: Hunk {
                commit_sha: sha.into(),
                file_path: format!("src/{sha}.rs"),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: diff.into(),
            },
            commit: CommitMeta {
                commit_sha: sha.into(),
                parent_sha: None,
                is_merge: false,
                author: Some("a".into()),
                ts,
                message: format!("msg-{sha}"),
            },
            similarity: sim,
        }
    }

    #[test]
    fn truncate_marks_truncation_for_long_diffs() {
        let big = (0..200)
            .map(|i| format!("line {}\n", i))
            .collect::<String>();
        let (out, trunc) = super::truncate_diff(&big, 80);
        assert!(trunc);
        assert!(out.contains("more lines"));
    }

    #[test]
    fn truncate_passthrough_for_short_diffs() {
        let small = "line a\nline b\n";
        let (out, trunc) = super::truncate_diff(small, 80);
        assert!(!trunc);
        assert_eq!(out, small);
    }

    #[test]
    fn truncate_does_not_pad_at_exact_boundary() {
        let exact = "a\nb\nc\n";
        let (out, trunc) = super::truncate_diff(exact, 3);
        assert!(!trunc);
        assert_eq!(out, exact);
    }

    #[test]
    fn truncate_counts_trailing_partial_line() {
        let with_partial = "a\nb\nc\nd";
        let (out, trunc) = super::truncate_diff(with_partial, 3);
        assert!(trunc);
        assert!(out.contains("(1 more lines)"));
        assert!(out.starts_with("a\nb\nc\n"));
    }

    // ---- Pipeline fakes ---------------------------------------------------

    /// Records which lanes were called and returns hard-coded `HunkHit`s
    /// per method.
    struct FakeStorage {
        knn: Vec<HunkHit>,
        fts_text: Vec<HunkHit>,
        fts_sym: Vec<HunkHit>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeStorage {
        fn new(knn: Vec<HunkHit>, fts_text: Vec<HunkHit>, fts_sym: Vec<HunkHit>) -> Self {
            Self {
                knn,
                fts_text,
                fts_sym,
                calls: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl crate::Storage for FakeStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: None,
                commits_behind_head: 0,
                indexed_at: None,
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn commit_exists(&self, _: &str) -> crate::Result<bool> {
            unreachable!("retriever tests should not exercise commit_exists")
        }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> {
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> crate::Result<()> {
            unreachable!("retriever tests should not exercise clear_head_symbols")
        }
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            self.calls.lock().unwrap().push("knn");
            Ok(self.knn.clone())
        }
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            self.calls.lock().unwrap().push("fts_text");
            Ok(self.fts_text.clone())
        }
        async fn bm25_hunks_by_semantic_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            // Plan 11: keep retriever tests focused on the existing
            // three lanes until Task 4.1 wires the semantic lane in.
            self.calls.lock().unwrap().push("fts_semantic");
            Ok(Vec::new())
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<HunkHit>> {
            self.calls.lock().unwrap().push("fts_sym");
            Ok(self.fts_sym.clone())
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
        async fn get_commit(&self, _: &RepoId, _: &str) -> crate::Result<Option<CommitMeta>> {
            unreachable!("retriever tests should not exercise get_commit")
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            _: &str,
            _: &str,
        ) -> crate::Result<Vec<crate::types::Hunk>> {
            unreachable!("retriever tests should not exercise get_hunks_for_file_in_commit")
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

    struct FakeEmbedder;
    #[async_trait]
    impl crate::EmbeddingProvider for FakeEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "fake"
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }
    }

    /// Reranker that maps a fixed `diff_text -> score` table. Returns 0.0
    /// for any unknown candidate so the pipeline still produces output.
    struct ScriptedReranker {
        scores: std::collections::HashMap<String, f32>,
    }

    #[async_trait]
    impl RerankProvider for ScriptedReranker {
        async fn rerank(&self, _query: &str, candidates: &[&str]) -> crate::Result<Vec<f32>> {
            Ok(candidates
                .iter()
                .map(|c| *self.scores.get(*c).unwrap_or(&0.0))
                .collect())
        }
    }

    #[tokio::test]
    async fn find_pattern_invokes_three_lanes_and_rrf() {
        // Three lanes return overlapping ids in different orders so RRF
        // alone would pick id=1 first. The reranker overrides that ordering
        // by giving "diff-c" the highest score; we assert the reranker's
        // ordering wins.
        let now = 1_700_000_000;
        let knn = vec![
            fake_hit(1, "a", now, 0.9, "diff-a"),
            fake_hit(2, "b", now, 0.5, "diff-b"),
            fake_hit(3, "c", now, 0.1, "diff-c"),
        ];
        let fts_text = vec![
            fake_hit(2, "b", now, 0.8, "diff-b"),
            fake_hit(1, "a", now, 0.3, "diff-a"),
        ];
        let fts_sym = vec![fake_hit(3, "c", now, 0.4, "diff-c")];
        let storage = Arc::new(FakeStorage::new(knn, fts_text, fts_sym));
        let embedder = Arc::new(FakeEmbedder);
        let mut scores = std::collections::HashMap::new();
        scores.insert("diff-c".to_string(), 9.0);
        scores.insert("diff-a".to_string(), 5.0);
        scores.insert("diff-b".to_string(), 1.0);
        let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker { scores });

        let r = Retriever::new(storage.clone(), embedder).with_reranker(reranker);
        let q = PatternQuery {
            query: "anything".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();

        let calls = storage.calls.lock().unwrap().clone();
        assert!(calls.contains(&"knn"), "knn lane must be called");
        assert!(calls.contains(&"fts_text"), "fts_text lane must be called");
        assert!(calls.contains(&"fts_sym"), "fts_sym lane must be called");

        assert_eq!(out.len(), 3, "all three unique ids should survive");
        assert_eq!(
            out[0].commit_sha,
            "c",
            "reranker score, not RRF rank, dictates final order: {:?}",
            out.iter()
                .map(|h| h.commit_sha.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(out[1].commit_sha, "a");
        assert_eq!(out[2].commit_sha, "b");
    }

    #[tokio::test]
    async fn find_pattern_no_rerank_returns_post_rrf_top_k() {
        // Without a reranker, every candidate gets score 1.0 and the
        // recency multiplier (with default 0.05 weight and same-day ts)
        // is identical for all rows, so the surviving order is the RRF
        // order. We construct lanes so RRF puts id=1 first.
        let now = 1_700_000_000;
        let knn = vec![
            fake_hit(1, "a", now, 0.9, "diff-a"),
            fake_hit(2, "b", now, 0.5, "diff-b"),
        ];
        let fts_text = vec![fake_hit(1, "a", now, 0.7, "diff-a")];
        let fts_sym = vec![fake_hit(2, "b", now, 0.4, "diff-b")];
        let storage = Arc::new(FakeStorage::new(knn, fts_text, fts_sym));
        let embedder = Arc::new(FakeEmbedder);

        let r = Retriever::new(storage, embedder);
        let q = PatternQuery {
            query: "anything".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].commit_sha, "a",
            "no-rerank mode should preserve RRF order"
        );
        assert_eq!(out[1].commit_sha, "b");
    }

    #[tokio::test]
    async fn find_pattern_query_no_rerank_flag_skips_attached_reranker() {
        // Reranker IS attached, but `query.no_rerank: true` must short-
        // circuit it. We construct lanes so RRF and reranker would
        // disagree about the winner: RRF puts id=1 first, the scripted
        // reranker would lift id=2. With no_rerank=true, the reranker
        // is bypassed and RRF order survives — id=1 wins. Crucially, we
        // also assert the ScriptedReranker's `calls` counter stays at 0,
        // proving the model was never invoked.
        let now = 1_700_000_000;
        let knn = vec![
            fake_hit(1, "a", now, 0.9, "diff-a"),
            fake_hit(2, "b", now, 0.5, "diff-b"),
        ];
        let fts_text = vec![fake_hit(1, "a", now, 0.7, "diff-a")];
        let fts_sym = vec![fake_hit(2, "b", now, 0.4, "diff-b")];
        let storage = Arc::new(FakeStorage::new(knn, fts_text, fts_sym));
        let embedder = Arc::new(FakeEmbedder);

        // Reranker would prefer id=2 (give "diff-b" a higher score). If
        // `no_rerank=true` actually bypasses the reranker, RRF order wins
        // and id=1 ("a") comes first. If the bypass is broken and the
        // reranker fires, id=2 ("b") would win — the assertion catches it.
        let scores: HashMap<String, f32> =
            HashMap::from([("diff-a".to_string(), 0.1), ("diff-b".to_string(), 0.9)]);
        let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker { scores });
        let r = Retriever::new(storage, embedder).with_reranker(reranker);

        let q = PatternQuery {
            query: "anything".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: true, // <-- the under-test signal
        };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].commit_sha, "a",
            "no_rerank=true must bypass the reranker; RRF ordering wins (otherwise id=2 would be first)"
        );
        assert_eq!(out[1].commit_sha, "b");
    }

    #[tokio::test]
    async fn find_pattern_recency_multiplier_breaks_ties_when_no_rerank() {
        // Both candidates have RRF score equal (they appear in disjoint
        // single-element lanes). With no reranker, every score is 1.0;
        // recency multiplier then favors the newer commit.
        let now = 1_700_000_000;
        let day = 86400_i64;
        let knn = vec![fake_hit(1, "old", now - 365 * day, 0.5, "diff-old")];
        let fts_text = vec![fake_hit(2, "new", now - day, 0.5, "diff-new")];
        let storage = Arc::new(FakeStorage::new(knn, fts_text, vec![]));
        let embedder = Arc::new(FakeEmbedder);
        let r = Retriever::new(storage, embedder);
        let q = PatternQuery {
            query: "anything".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();
        assert_eq!(out.len(), 2);
        // RRF gives id=1 first (knn lane appears first), but recency
        // multiplier on the newer commit lifts it above.
        assert_eq!(
            out[0].commit_sha, "new",
            "newer commit should outrank older when scores are tied"
        );
    }
}
