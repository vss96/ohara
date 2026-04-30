use crate::query::PatternHit;
use crate::storage::HunkHit;
use crate::types::{CommitMeta, Hunk, Provenance};
use chrono::{DateTime, Utc};
use std::sync::Arc;

pub struct RankingWeights {
    pub similarity: f32,
    pub recency: f32,
    pub message_match: f32,
    pub recency_half_life_days: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self { similarity: 0.7, recency: 0.2, message_match: 0.1, recency_half_life_days: 365.0 }
    }
}

pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
}

impl Retriever {
    pub fn new(storage: Arc<dyn crate::Storage>, embedder: Arc<dyn crate::EmbeddingProvider>) -> Self {
        Self { weights: RankingWeights::default(), storage, embedder }
    }

    pub fn with_weights(mut self, w: RankingWeights) -> Self {
        self.weights = w;
        self
    }

    /// Pure ranking step, separated for testability.
    pub fn rank_hits(
        &self,
        hits: Vec<HunkHit>,
        message_similarities: &[f32],
        now_unix: i64,
    ) -> Vec<PatternHit> {
        assert_eq!(hits.len(), message_similarities.len());
        let mut out: Vec<PatternHit> = hits
            .into_iter()
            .zip(message_similarities.iter())
            .map(|(h, &msg_sim)| {
                let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86400.0;
                let recency = (-age_days / self.weights.recency_half_life_days).exp();
                let combined = self.weights.similarity * h.similarity
                    + self.weights.recency * recency
                    + self.weights.message_match * msg_sim;
                let date = DateTime::<Utc>::from_timestamp(h.commit.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                let (excerpt, truncated) = truncate_diff(&h.hunk.diff_text, 80);
                PatternHit {
                    commit_sha: h.commit.sha,
                    commit_message: h.commit.message,
                    commit_author: h.commit.author,
                    commit_date: date,
                    file_path: h.hunk.file_path,
                    change_kind: format!("{:?}", h.hunk.change_kind).to_lowercase(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    related_head_symbols: vec![],   // populated in a later plan if symbol attribution is added
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
        out
    }
}

fn truncate_diff(s: &str, max_lines: usize) -> (String, bool) {
    // Count total lines, treating a trailing partial line (no \n) as a line.
    let nl = s.bytes().filter(|&b| b == b'\n').count();
    let has_trailing_partial = !s.is_empty() && !s.ends_with('\n');
    let total_lines = nl + if has_trailing_partial { 1 } else { 0 };

    if total_lines <= max_lines {
        return (s.to_string(), false);
    }

    // Find byte index of the end of line `max_lines`.
    let mut end = 0;
    let mut count = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == max_lines {
                end = i + 1;
                break;
            }
        }
    }

    let extra = total_lines - max_lines;
    let mut out = s[..end].to_string();
    out.push_str(&format!("... ({} more lines)\n", extra));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangeKind;

    fn fake_hit(sha: &str, ts: i64, sim: f32, diff: &str) -> HunkHit {
        HunkHit {
            hunk: Hunk {
                commit_sha: sha.into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: diff.into(),
            },
            commit: CommitMeta {
                sha: sha.into(),
                parent_sha: None,
                is_merge: false,
                author: Some("a".into()),
                ts,
                message: "m".into(),
            },
            similarity: sim,
        }
    }

    struct PanicStorage;
    #[async_trait::async_trait]
    impl crate::Storage for PanicStorage {
        async fn open_repo(&self, _: &crate::types::RepoId, _: &str, _: &str) -> crate::Result<()> { unreachable!() }
        async fn get_index_status(&self, _: &crate::types::RepoId) -> crate::Result<crate::query::IndexStatus> { unreachable!() }
        async fn set_last_indexed_commit(&self, _: &crate::types::RepoId, _: &str) -> crate::Result<()> { unreachable!() }
        async fn put_commit(&self, _: &crate::types::RepoId, _: &crate::CommitRecord) -> crate::Result<()> { unreachable!() }
        async fn put_hunks(&self, _: &crate::types::RepoId, _: &[crate::HunkRecord]) -> crate::Result<()> { unreachable!() }
        async fn put_head_symbols(&self, _: &crate::types::RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { unreachable!() }
        async fn knn_hunks(&self, _: &crate::types::RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<crate::HunkHit>> { unreachable!() }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { unreachable!() }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { unreachable!() }
    }

    struct PanicEmbedder;
    #[async_trait::async_trait]
    impl crate::EmbeddingProvider for PanicEmbedder {
        fn dimension(&self) -> usize { unreachable!() }
        fn model_id(&self) -> &str { unreachable!() }
        async fn embed_batch(&self, _: &[String]) -> crate::Result<Vec<Vec<f32>>> { unreachable!() }
    }

    fn retriever_for_test() -> Retriever {
        Retriever {
            weights: RankingWeights::default(),
            storage: Arc::new(PanicStorage),
            embedder: Arc::new(PanicEmbedder),
        }
    }

    #[test]
    fn rank_orders_higher_similarity_first_when_recency_equal() {
        let now = 1_700_000_000;
        let hits = vec![
            fake_hit("a", now - 86400, 0.5, "+x"),
            fake_hit("b", now - 86400, 0.9, "+y"),
        ];
        let msg_sims = vec![0.0, 0.0];
        let out = retriever_for_test().rank_hits(hits, &msg_sims, now);
        assert_eq!(out[0].commit_sha, "b");
        assert_eq!(out[1].commit_sha, "a");
        assert!(out[0].combined_score > out[1].combined_score);
    }

    #[test]
    fn truncate_marks_truncation_for_long_diffs() {
        let big = (0..200).map(|i| format!("line {}\n", i)).collect::<String>();
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
        // When input has exactly max_lines lines and ends with newline, no truncation.
        let exact = "a\nb\nc\n";
        let (out, trunc) = super::truncate_diff(exact, 3);
        assert!(!trunc);
        assert_eq!(out, exact);
    }

    #[test]
    fn truncate_counts_trailing_partial_line() {
        // Input has 3 newlines + a trailing partial line ("d"). Total = 4 lines.
        // With max_lines=3, expect truncation reporting 1 more line elided.
        let with_partial = "a\nb\nc\nd";
        let (out, trunc) = super::truncate_diff(with_partial, 3);
        assert!(trunc);
        assert!(out.contains("(1 more lines)"));
        assert!(out.starts_with("a\nb\nc\n"));
    }

    #[test]
    fn rank_does_not_panic_on_nan_scores() {
        let now = 1_700_000_000;
        // Create hits with f32::NAN as the similarity. The combined_score formula
        // includes 0.7 * similarity, which propagates NaN.
        let hits = vec![
            fake_hit("a", now, f32::NAN, "+x"),
            fake_hit("b", now, 0.5, "+y"),
        ];
        let msg_sims = vec![0.0, 0.0];
        // Should NOT panic. Order of NaN entries is implementation-defined (Equal).
        let out = retriever_for_test().rank_hits(hits, &msg_sims, now);
        assert_eq!(out.len(), 2);
        // The non-NaN entry should still have a finite combined_score.
        let finite_count = out.iter().filter(|h| h.combined_score.is_finite()).count();
        assert_eq!(finite_count, 1);
    }
}
