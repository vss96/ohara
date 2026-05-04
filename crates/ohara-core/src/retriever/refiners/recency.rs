#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::RankingWeights;
    use crate::storage::{HunkHit, HunkId};

    fn make_hit(id: HunkId, ts: i64) -> HunkHit {
        use crate::types::{ChangeKind, CommitMeta, Hunk};
        HunkHit {
            hunk_id: id,
            hunk: Hunk { commit_sha: "x".into(), file_path: "f.rs".into(), language: None, change_kind: ChangeKind::Added, diff_text: "diff".into() },
            commit: CommitMeta { commit_sha: "x".into(), parent_sha: None, is_merge: false, author: None, ts, message: "m".into() },
            similarity: 1.0,
        }
    }

    #[tokio::test]
    async fn recency_refiner_newer_hit_ranks_higher() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(2, now - 100 * day), // older
            make_hit(1, now - day),       // newer
        ];
        let weights = RankingWeights::default();
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].hunk_id, 1,
            "newer hit (1 day old) must outrank older (100 days old)"
        );
        assert_eq!(out[1].hunk_id, 2);
    }

    #[tokio::test]
    async fn recency_refiner_zero_weight_preserves_input_order() {
        let now = 1_700_000_000_i64;
        let day = 86_400_i64;
        let hits = vec![
            make_hit(10, now - day),
            make_hit(11, now - 200 * day),
        ];
        let mut weights = RankingWeights::default();
        weights.recency_weight = 0.0; // disable tie-break
        let refiner = RecencyRefiner::new(weights, now);
        let out = refiner.refine("q", hits).await.unwrap();
        assert_eq!(out[0].hunk_id, 10);
        assert_eq!(out[1].hunk_id, 11);
    }
}
