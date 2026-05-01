use crate::query::PatternHit;
use crate::storage::HunkHit;
use crate::types::Provenance;
#[cfg(test)]
use crate::types::{CommitMeta, Hunk};
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
        Self {
            similarity: 0.7,
            recency: 0.2,
            message_match: 0.1,
            recency_half_life_days: 365.0,
        }
    }
}

pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
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
        }
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
                    related_head_symbols: vec![], // populated in a later plan if symbol attribution is added
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

impl Retriever {
    pub async fn find_pattern(
        &self,
        repo_id: &crate::types::RepoId,
        query: &crate::query::PatternQuery,
        now_unix: i64,
    ) -> crate::Result<Vec<crate::query::PatternHit>> {
        let q_text = vec![query.query.clone()];
        let mut q_embs = self.embedder.embed_batch(&q_text).await?;
        let q_emb = q_embs
            .pop()
            .ok_or_else(|| crate::OhraError::Embedding("empty".into()))?;

        let candidates = self
            .storage
            .knn_hunks(
                repo_id,
                &q_emb,
                query.k.clamp(1, 20),
                query.language.as_deref(),
                query.since_unix,
            )
            .await?;

        // Cosine similarity between the query embedding and each candidate's commit message.
        // We embed the messages in a single batch.
        let messages: Vec<String> = candidates
            .iter()
            .map(|h| h.commit.message.clone())
            .collect();
        let msg_embs = if messages.is_empty() {
            vec![]
        } else {
            self.embedder.embed_batch(&messages).await?
        };
        let msg_sims: Vec<f32> = msg_embs.iter().map(|e| cosine(&q_emb, e)).collect();

        Ok(self.rank_hits(candidates, &msg_sims, now_unix))
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
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
        async fn open_repo(&self, _: &crate::types::RepoId, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
        async fn get_index_status(
            &self,
            _: &crate::types::RepoId,
        ) -> crate::Result<crate::query::IndexStatus> {
            unreachable!()
        }
        async fn set_last_indexed_commit(
            &self,
            _: &crate::types::RepoId,
            _: &str,
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn put_commit(
            &self,
            _: &crate::types::RepoId,
            _: &crate::CommitRecord,
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn put_hunks(
            &self,
            _: &crate::types::RepoId,
            _: &[crate::HunkRecord],
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn put_head_symbols(
            &self,
            _: &crate::types::RepoId,
            _: &[crate::types::Symbol],
        ) -> crate::Result<()> {
            unreachable!()
        }
        async fn knn_hunks(
            &self,
            _: &crate::types::RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> crate::Result<Vec<crate::HunkHit>> {
            unreachable!()
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
            unreachable!()
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
            unreachable!()
        }
    }

    struct PanicEmbedder;
    #[async_trait::async_trait]
    impl crate::EmbeddingProvider for PanicEmbedder {
        fn dimension(&self) -> usize {
            unreachable!()
        }
        fn model_id(&self) -> &str {
            unreachable!()
        }
        async fn embed_batch(&self, _: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            unreachable!()
        }
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

    use crate::query::PatternQuery;
    use crate::storage::{CommitRecord, HunkRecord};
    use crate::types::{RepoId, Symbol};

    struct FakeEmbedder;
    #[async_trait::async_trait]
    impl crate::EmbeddingProvider for FakeEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        fn model_id(&self) -> &str {
            "fake"
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| match t.as_str() {
                    "retry" => vec![1.0, 0.0, 0.0, 0.0],
                    "added retry logic" => vec![1.0, 0.1, 0.0, 0.0],
                    "renamed file" => vec![0.0, 1.0, 0.0, 0.0],
                    _ => vec![0.0; 4],
                })
                .collect())
        }
    }

    struct FakeStorage {
        hits: Vec<HunkHit>,
    }
    #[async_trait::async_trait]
    impl crate::Storage for FakeStorage {
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
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> {
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> {
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
            Ok(self.hits.clone())
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> {
            Ok(())
        }
    }

    fn fake_hit_with_msg(sha: &str, ts: i64, sim: f32, msg: &str) -> HunkHit {
        HunkHit {
            hunk: Hunk {
                commit_sha: sha.into(),
                file_path: "a.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+x".into(),
            },
            commit: CommitMeta {
                sha: sha.into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts,
                message: msg.into(),
            },
            similarity: sim,
        }
    }

    #[tokio::test]
    async fn find_pattern_message_match_breaks_ties() {
        let now = 1_700_000_000;
        let storage = Arc::new(FakeStorage {
            hits: vec![
                fake_hit_with_msg("a", now - 86400, 0.8, "added retry logic"),
                fake_hit_with_msg("b", now - 86400, 0.8, "renamed file"),
            ],
        });
        let embedder = Arc::new(FakeEmbedder);
        let r = Retriever::new(storage, embedder);
        let q = PatternQuery {
            query: "retry".into(),
            k: 5,
            language: None,
            since_unix: None,
        };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();
        assert_eq!(
            out[0].commit_sha, "a",
            "retry-related commit message should win the tie"
        );
    }
}
