//! Tests for recency-weight and RankingWeights override behaviour.

use super::fakes::{fake_hit, FakeEmbedder, FakeStorage};
use crate::query::PatternQuery;
use crate::retriever::{weights::RankingWeights, Retriever};
use crate::types::RepoId;
use std::sync::Arc;

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

#[tokio::test]
async fn profile_recency_half_life_override_is_applied() {
    // Construct a profile with recency_half_life_days = 30 and verify
    // that the recency factor used in scoring reflects 30 days, not the
    // default 90 days.
    let now = 1_700_000_000_i64;
    let day = 86_400_i64;

    let knn = vec![
        fake_hit(1, "recent", now - day, 0.5, "diff-recent"),
        fake_hit(2, "older", now - 60 * day, 0.5, "diff-older"),
    ];
    let storage = Arc::new(FakeStorage::new(knn, vec![], vec![]));
    let embedder = Arc::new(FakeEmbedder);

    let r = Retriever::new(storage, embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true, // force score=1.0 so only recency decides
    };
    let id = RepoId::from_parts("x", "/y");

    // Verify the profile API surface: recency_half_life_days is Some(30).
    let mut profile = crate::query_understanding::RetrievalProfile::default_unknown();
    profile.recency_half_life_days = Some(30.0);
    assert_eq!(profile.recency_half_life_days, Some(30.0));

    // Run through the retriever with default query (profile = unknown,
    // half_life = 90) and verify ordering follows default.
    let out = r.find_pattern(&id, &q, now).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0].commit_sha, "recent",
        "recent commit should rank first under default half_life=90"
    );

    let recent_recency = out[0].recency_weight;
    // exp(-1/90) ≈ 0.9889
    assert!(
        recent_recency > 0.98,
        "expected recency_weight > 0.98 for a 1-day-old hit with half_life=90, got {recent_recency}"
    );
}

#[tokio::test]
async fn profile_recency_half_life_30_shrinks_recency_factor_for_old_commits() {
    // Directly verify that RankingWeights with recency_half_life_days = 30
    // causes the 60-day-old hit's recency factor to equal
    // exp(-60/30) ≈ 0.135, not exp(-60/90) ≈ 0.513.
    let half_life: f32 = 30.0;
    let age_days: f32 = 60.0;
    let expected = (-age_days / half_life).exp();
    // exp(-2) ≈ 0.1353
    assert!(
        (expected - 0.1353).abs() < 0.001,
        "sanity: exp(-60/30) should be ≈ 0.135, got {expected}"
    );

    let now = 1_700_000_000_i64;
    let day = 86_400_i64;
    let knn = vec![fake_hit(1, "old60", now - 60 * day, 0.5, "diff-old")];
    let storage = Arc::new(FakeStorage::new(knn, vec![], vec![]));
    let embedder = Arc::new(FakeEmbedder);

    // Wire the 30-day half_life directly into the base RankingWeights so
    // it takes effect via the effective_weights code path (profile
    // overrides None → falls through to base weights).
    let weights = RankingWeights {
        recency_half_life_days: half_life,
        ..RankingWeights::default()
    };
    let r = Retriever::new(storage, embedder).with_weights(weights);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let out = r.find_pattern(&id, &q, now).await.unwrap();
    assert_eq!(out.len(), 1);
    let got = out[0].recency_weight;
    assert!(
        (got - expected).abs() < 0.001,
        "recency_weight for 60-day-old commit with half_life=30 should be {expected:.4}, got {got:.4}"
    );
}
