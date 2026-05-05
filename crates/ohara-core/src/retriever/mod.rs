//! Retrieval pipeline.
//!
//! Three lanes — vector KNN, BM25 over hunk text, BM25 over symbol names —
//! gather candidates in parallel; Reciprocal Rank Fusion (`k = 60`) merges
//! the lanes; an optional cross-encoder rerank scores the surviving
//! candidates against the query; a small recency multiplier acts as a
//! tie-breaker on the rerank score.

pub mod lanes;
pub use lanes::{LaneId, RetrievalLane};

pub mod refiners;
pub use refiners::ScoreRefiner;

pub mod coordinator;

pub mod weights;
pub use weights::RankingWeights;

use crate::diff_text::{truncate_diff, DIFF_EXCERPT_MAX_LINES};
use crate::embed::RerankProvider;
use crate::perf_trace::timed_phase;
use crate::query::{PatternHit, PatternQuery};
use crate::storage::HunkId;
use crate::types::Provenance;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

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
        // Existing entry-point — discards the profile metadata. CLI
        // and MCP callers prefer `find_pattern_with_profile`.
        self.find_pattern_with_profile(repo_id, query, now_unix)
            .await
            .map(|(hits, _profile)| hits)
    }

    /// Plan 12 Task 2.1 / Plan 20: same as [`find_pattern`] but also returns
    /// the [`RetrievalProfile`] picked from
    /// `query_understanding::parse_query`. Lets callers surface the
    /// profile in their response metadata (`_meta.query_profile`)
    /// without re-running the parser.
    ///
    /// Plan 20: body replaced with coordinator-based pipeline. Lanes and
    /// refiners are constructed per call (Arc clone is O(1)); the public
    /// API is unchanged.
    pub async fn find_pattern_with_profile(
        &self,
        repo_id: &crate::types::RepoId,
        query: &PatternQuery,
        now_unix: i64,
    ) -> crate::Result<(
        Vec<PatternHit>,
        crate::query_understanding::RetrievalProfile,
    )> {
        use crate::retriever::coordinator;
        use crate::retriever::lanes::{
            bm25_head_sym::Bm25HeadSymLane, bm25_hist_sym::Bm25HistSymLane,
            bm25_sem_text::Bm25SemTextLane, bm25_text::Bm25TextLane, vec::VecLane, RetrievalLane,
        };
        use crate::retriever::refiners::{
            cross_encoder::CrossEncoderRefiner, recency::RecencyRefiner, ScoreRefiner,
        };

        let parsed = crate::query_understanding::parse_query(&query.query);
        let profile = crate::query_understanding::RetrievalProfile::for_intent(parsed.intent);

        // Apply per-profile RankingWeights overrides (preserves
        // recency_half_life_days and lane_top_k overrides from the profile).
        let effective_weights = RankingWeights {
            recency_weight: profile
                .recency_weight
                .unwrap_or(self.weights.recency_weight),
            recency_half_life_days: profile
                .recency_half_life_days
                .unwrap_or(self.weights.recency_half_life_days),
            rerank_top_k: profile.rerank_top_k.unwrap_or(self.weights.rerank_top_k),
            lane_top_k: profile.lane_top_k.unwrap_or(self.weights.lane_top_k),
        };

        let rerank_top_k = effective_weights.rerank_top_k;

        // Build lanes (profile-gating is inside each lane via is_lane_enabled).
        // Plan 25: 5 lanes now — vec / bm25_text (raw diff_text) /
        // bm25_sem_text (contextual hunk.semantic_text) / bm25_hist_sym
        // / bm25_head_sym. All five fuse into the same RRF call.
        let lanes: Vec<Box<dyn RetrievalLane>> = vec![
            Box::new(VecLane::new(self.storage.clone(), self.embedder.clone())),
            Box::new(Bm25TextLane::new(self.storage.clone())),
            Box::new(Bm25SemTextLane::new(self.storage.clone())),
            Box::new(Bm25HistSymLane::new(self.storage.clone())),
            Box::new(Bm25HeadSymLane::new(self.storage.clone())),
        ];

        // Build refiners. Fold the profile's recency multiplier into the weight
        // so RecencyRefiner is self-contained.
        let effective_recency_weight =
            effective_weights.recency_weight * profile.recency_multiplier;
        let mut recency_weights = effective_weights.clone();
        recency_weights.recency_weight = effective_recency_weight;

        let mut refiners: Vec<Box<dyn ScoreRefiner>> = Vec::new();
        if let Some(reranker) = &self.reranker {
            if !query.no_rerank {
                refiners.push(Box::new(CrossEncoderRefiner::new(reranker.clone())));
            }
        }
        refiners.push(Box::new(RecencyRefiner::new(recency_weights, now_unix)));

        // Run coordinator.
        let final_k = query.k.clamp(1, 20) as usize;
        let raw_hits =
            coordinator::run(&lanes, &refiners, query, repo_id, rerank_top_k, final_k).await?;

        if raw_hits.is_empty() {
            return Ok((vec![], profile));
        }

        // Hydrate per-hunk symbol attribution rows.
        //
        // Plan 24: one batch call replacing the N sequential per-hit
        // round-trips. Storage seeds every requested hunk_id in the
        // returned map (with an empty Vec when no attribution rows
        // exist), so the post-filter `is_empty` check below preserves
        // the legacy "absent ⇒ no related_head_symbols" behaviour.
        let hunk_ids: Vec<HunkId> = raw_hits.iter().map(|h| h.hunk_id).collect();
        let symbols_by_hunk: HashMap<HunkId, Vec<String>> = timed_phase("hydrate_symbols", async {
            let attrs_map = self
                .storage
                .get_hunk_symbols_batch(repo_id, &hunk_ids)
                .await?;
            Ok::<_, crate::OhraError>(
                attrs_map
                    .into_iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(id, v)| (id, v.into_iter().map(|a| a.name).collect()))
                    .collect(),
            )
        })
        .await?;

        // Map HunkHit → PatternHit.
        let out: Vec<PatternHit> = raw_hits
            .into_iter()
            .map(|h| {
                let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86_400.0;
                let recency = (-age_days / effective_weights.recency_half_life_days).exp();
                let date = DateTime::<Utc>::from_timestamp(h.commit.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                let (excerpt, truncated) = truncate_diff(&h.hunk.diff_text, DIFF_EXCERPT_MAX_LINES);
                let related_head_symbols =
                    symbols_by_hunk.get(&h.hunk_id).cloned().unwrap_or_default();
                PatternHit {
                    commit_sha: h.commit.commit_sha,
                    commit_message: h.commit.message,
                    commit_author: h.commit.author,
                    commit_date: date,
                    file_path: h.hunk.file_path,
                    change_kind: format!("{:?}", h.hunk.change_kind).to_lowercase(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    related_head_symbols,
                    similarity: h.similarity,
                    recency_weight: recency,
                    combined_score: h.similarity, // coordinator already applied recency
                    provenance: Provenance::Inferred,
                }
            })
            .collect();

        Ok((out, profile))
    }
}

#[cfg(test)]
mod tests;
