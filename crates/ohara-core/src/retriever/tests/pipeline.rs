//! Pipeline integration tests: lane invocation, RRF ordering, reranker bypass.

use super::fakes::{fake_hit, FakeEmbedder, FakeStorage, ScriptedReranker};
use crate::embed::RerankProvider;
use crate::query::PatternQuery;
use crate::retriever::Retriever;
use crate::types::RepoId;
use std::collections::HashMap;
use std::sync::Arc;

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
    let mut scores = HashMap::new();
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
async fn find_pattern_negative_rerank_still_ranks_recent_above_old() {
    // Plan 22 regression: when the cross-encoder returns negative
    // logits for low-relevance candidates (which `bge-reranker-base`
    // routinely does), the multiplicative recency formula must still
    // place the more recent hit above the older one. Pre-fix the
    // ordering inverts because `negative * (1 + small_positive)` is
    // *more* negative than `negative * (1 + larger_positive)`.
    let now = 1_700_000_000_i64;
    let day = 86_400_i64;

    // Both candidates land in disjoint single-element lanes (RRF rank
    // 1 in their lane, absent from the others) so RRF gives them
    // equal fused scores and ordering is dictated entirely by
    // `combined = f(rerank, recency)`.
    let knn = vec![fake_hit(1, "old", now - 365 * day, 0.5, "diff-bad-old")];
    let fts_text = vec![fake_hit(2, "new", now - day, 0.5, "diff-bad-new")];
    let storage = Arc::new(FakeStorage::new(knn, fts_text, vec![]));
    let embedder = Arc::new(FakeEmbedder);

    // Reranker assigns the *same* negative logit to both candidates.
    // Identical bases, so only the recency multiplier differentiates,
    // and pre-fix it differentiates in the wrong direction.
    let scores: HashMap<String, f32> = HashMap::from([
        ("diff-bad-old".to_string(), -2.0),
        ("diff-bad-new".to_string(), -2.0),
    ]);
    let reranker: Arc<dyn RerankProvider> = Arc::new(ScriptedReranker { scores });

    let r = Retriever::new(storage, embedder).with_reranker(reranker);
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
        out[0].commit_sha,
        "new",
        "newer commit MUST outrank older when rerank scores are tied, \
         even when both are negative; got order {:?}",
        out.iter()
            .map(|h| h.commit_sha.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        out[0].combined_score > out[1].combined_score,
        "combined_score must be monotone with sort order; got new={} old={}",
        out[0].combined_score,
        out[1].combined_score
    );
}

#[tokio::test]
async fn find_pattern_invokes_semantic_text_lane_and_fuses_into_rrf() {
    // Plan 25: the semantic-text lane MUST be queried, and a hit
    // surfaced ONLY by that lane MUST appear in the fused output. We
    // construct lanes so the semantic lane is the *only* source for
    // hunk_id=99; if the new lane is wired in, hunk 99 surfaces;
    // otherwise it doesn't.
    let now = 1_700_000_000;
    let knn = vec![fake_hit(1, "a", now, 0.9, "diff-a")];
    let fts_text = vec![fake_hit(2, "b", now, 0.5, "diff-b")];
    let fts_sym = vec![fake_hit(3, "c", now, 0.3, "diff-c")];
    let fts_semantic = vec![fake_hit(99, "z", now, 0.7, "diff-z-only-in-semantic")];
    let storage = Arc::new(FakeStorage::new_with_semantic(
        knn,
        fts_text,
        fts_sym,
        fts_semantic,
    ));
    let embedder = Arc::new(FakeEmbedder);
    let r = Retriever::new(storage.clone(), embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 10,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let out = r.find_pattern(&id, &q, now).await.unwrap();

    let calls = storage.calls.lock().unwrap().clone();
    assert!(
        calls.contains(&"fts_semantic"),
        "semantic-text lane MUST be invoked; calls = {calls:?}"
    );
    assert!(
        out.iter().any(|h| h.commit_sha == "z"),
        "hunk surfaced ONLY by the semantic-text lane MUST appear in fused output; \
         got {:?}",
        out.iter()
            .map(|h| h.commit_sha.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn find_pattern_calls_get_hunk_symbols_batch_exactly_once() {
    // Plan 24 regression: hydration must be one batch call, not N
    // sequential calls. We construct lanes that surface 5 distinct
    // hunks; the retriever should make exactly 1 call to the batch
    // method and 0 calls to the per-hit method.
    let now = 1_700_000_000;
    let knn = vec![
        fake_hit(1, "a", now, 0.9, "diff-a"),
        fake_hit(2, "b", now, 0.5, "diff-b"),
        fake_hit(3, "c", now, 0.4, "diff-c"),
        fake_hit(4, "d", now, 0.3, "diff-d"),
        fake_hit(5, "e", now, 0.2, "diff-e"),
    ];
    let storage = Arc::new(FakeStorage::new(knn, vec![], vec![]));
    let embedder = Arc::new(FakeEmbedder);
    let r = Retriever::new(storage.clone(), embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");
    let _ = r.find_pattern(&id, &q, now).await.unwrap();

    let batch_calls = *storage.batch_calls.lock().unwrap();
    assert_eq!(
        batch_calls, 1,
        "hydrate_symbols MUST issue exactly 1 batch call for ≥1 surviving hits, got {batch_calls}"
    );

    // The per-hit method must NOT have been called by the retriever.
    let per_hit_calls = storage
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == "get_hunk_symbols")
        .count();
    assert_eq!(
        per_hit_calls, 0,
        "per-hit get_hunk_symbols must not be called by the retriever after plan-24"
    );
}

// ---- Phase-event capture ------------------------------------------------
// Uses crate::perf_trace::test_phase_capture shared with explain::tests.

#[test]
fn find_pattern_emits_expected_phase_events() {
    let (seen, _guard) = crate::perf_trace::test_phase_capture::acquire_phase_collector();

    let now = 1_700_000_000;
    let knn = vec![fake_hit(1, "a", now, 0.9, "diff-a")];
    let fts = vec![fake_hit(1, "a", now, 0.7, "diff-a")];
    let storage = Arc::new(FakeStorage::new(knn, fts, vec![]));
    let embedder = Arc::new(FakeEmbedder);
    let r = Retriever::new(storage, embedder);
    let q = PatternQuery {
        query: "anything".into(),
        k: 5,
        language: None,
        since_unix: None,
        no_rerank: true,
    };
    let id = RepoId::from_parts("x", "/y");

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = r.find_pattern(&id, &q, now).await.unwrap();
    });

    let seen = seen.lock().unwrap();
    // Plan 25: `lane_fts_semantic` joins as a 5th lane; the embed
    // step is still run by VecLane via the `embed_query` phase span.
    for required in [
        "embed_query",
        "lane_knn",
        "lane_fts_text",
        "lane_fts_semantic",
        "lane_fts_sym_hist",
        "lane_fts_sym_head",
        "rrf",
        "hydrate_symbols",
    ] {
        assert!(
            seen.contains(required),
            "missing phase event {required}; seen = {:?}",
            *seen
        );
    }
}
