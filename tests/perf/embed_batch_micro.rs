//! Issue #56 — embed-batch microbench.
//!
//! Measures throughput of `FastEmbedProvider::embed_batch` at varying
//! per-call batch sizes against the BGE-small full-precision model.
//! Bypasses the indexer / git / storage paths so we see the pure
//! ONNX forward-pass + rayon-internal-batching curve.
//!
//! Workload: a fixed 4096-string corpus of synthetic "code-shaped"
//! semantic-text rows (mirrors the shape `EmbedStage` builds). Each
//! sweep cell embeds the entire corpus by repeatedly calling
//! `embed_batch(&corpus[i..i+B])` until exhausted.
//!
//! Run:
//!   cargo build --release -p ohara-cli
//!   cargo test -p ohara-perf-tests --release -- \
//!       --ignored embed_batch_micro --nocapture
//!
//! Operator-run; not in CI. `#[ignore]`'d for the same reason the
//! rest of `tests/perf/` is.

use ohara_core::EmbeddingProvider;
use ohara_embed::FastEmbedProvider;
use std::time::Instant;

const BATCHES: &[usize] = &[16, 32, 64, 128, 256];
const CORPUS_SIZE: usize = 4096;
const ITERATIONS: usize = 3;

/// Build a synthetic corpus that roughly resembles the `semantic-text`
/// rows EmbedStage produces (keyword-ish, mixed length, code-flavoured).
/// Deterministic so runs are comparable.
fn build_corpus() -> Vec<String> {
    let snippets = [
        "fn parse_commit(repo: &Repository, oid: Oid) -> Result<Commit>",
        "impl EmbeddingProvider for FastEmbedProvider { async fn embed_batch }",
        "select id, sha, message, author, ts from commit where repo_id = ?",
        "tokio::spawn_blocking move guard model embed refs",
        "pub struct Indexer<S: Storage, E: EmbeddingProvider>",
        "match diff.kind() { DeltaType::Added => { ... } DeltaType::Modified => { ... } }",
        "BEGIN IMMEDIATE; INSERT INTO vec_commit(rowid, embedding) VALUES (?,?)",
        "let chunks = parse_chunks(&blob, lang)?; let texts = chunks.iter().map(...)",
        "fn rrf_fuse(a: &[Hit], b: &[Hit], c: &[Hit], k: usize) -> Vec<Hit>",
        "embed_batch returned {} vectors for {} inputs",
        "use refinery::embed_migrations; embed_migrations!(\"./migrations\")",
        "tracing::info!(target: \"ohara::phase\", phase = \"embed\", elapsed_ms);",
        "for chunk in texts.chunks(cap) { let embs = embedder.embed_batch(chunk).await?; }",
        "RankingWeights { vector: 0.55, fts_hunk: 0.25, fts_symbol: 0.20 }",
        "pub fn pick_resources(host: &Host) -> ResourcePlan",
        "let mut stmt = conn.prepare_cached(\"SELECT * FROM hunk WHERE commit_id = ?\")?;",
    ];
    let mut corpus = Vec::with_capacity(CORPUS_SIZE);
    for i in 0..CORPUS_SIZE {
        let s = snippets[i % snippets.len()];
        // Append a unique tail so dedup-style optimisations don't kick in.
        corpus.push(format!("{} // row #{}", s, i));
    }
    corpus
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf microbench — opt in via --ignored embed_batch_micro --nocapture"]
async fn embed_batch_microbench() {
    let provider = FastEmbedProvider::new().expect("init FastEmbedProvider (CPU)");
    let corpus = build_corpus();

    // Warm-up: single small batch to prime ONNX session + rayon pool.
    let _ = provider.embed_batch(&corpus[..16]).await.expect("warmup");

    eprintln!(
        "\n=== embed_batch microbench — corpus_size={}, iterations={} ===",
        CORPUS_SIZE, ITERATIONS
    );
    eprintln!(
        "{:>6} | {:>10} | {:>12} | {:>10}",
        "batch", "iter", "elapsed_ms", "thr (rps)"
    );

    let mut by_batch: Vec<(usize, Vec<u128>)> = Vec::new();
    for &b in BATCHES {
        let mut samples = Vec::with_capacity(ITERATIONS);
        for it in 0..ITERATIONS {
            let start = Instant::now();
            let mut i = 0;
            while i < corpus.len() {
                let end = (i + b).min(corpus.len());
                let chunk = corpus[i..end].to_vec();
                let _ = provider.embed_batch(&chunk).await.expect("embed_batch");
                i = end;
            }
            let ms = start.elapsed().as_millis();
            let rps = (CORPUS_SIZE as f64 / (ms as f64 / 1000.0)) as u64;
            eprintln!("{:>6} | {:>10} | {:>12} | {:>10}", b, it, ms, rps);
            samples.push(ms);
        }
        by_batch.push((b, samples));
    }

    // Pretty summary table.
    eprintln!("\n=== summary (median across {} iters) ===", ITERATIONS);
    eprintln!(
        "{:>6} | {:>12} | {:>12} | {:>12} | {:>9} | {:>9}",
        "batch", "min_ms", "median_ms", "max_ms", "thr (rps)", "vs 32"
    );
    let mut median_at_32: Option<u128> = None;
    for (b, samples) in &by_batch {
        let mut s = samples.clone();
        s.sort_unstable();
        let mid = s[s.len() / 2];
        if *b == 32 {
            median_at_32 = Some(mid);
        }
    }
    for (b, samples) in &by_batch {
        let mut s = samples.clone();
        s.sort_unstable();
        let mn = *s.first().unwrap();
        let mid = s[s.len() / 2];
        let mx = *s.last().unwrap();
        let rps = (CORPUS_SIZE as f64 / (mid as f64 / 1000.0)) as u64;
        let speedup = match median_at_32 {
            Some(base) => format!("{:>4.2}x", base as f64 / mid as f64),
            None => "?".to_string(),
        };
        eprintln!(
            "{:>6} | {:>12} | {:>12} | {:>12} | {:>9} | {:>9}",
            b, mn, mid, mx, rps, speedup
        );
    }
}
