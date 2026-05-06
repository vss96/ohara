//! Issue #57 perf microbench: symbol-name BM25 lane against a
//! synthetic high-fan-out fixture. Operator-run; not in CI.
//!
//! The lane joins `fts_symbol_name` to every hunk that ever touched
//! the matched symbol's file, so a single matched symbol can fan out
//! to thousands of rows on hot files. The pre-fix SQL had no `LIMIT`
//! on the inner query; SQLite's `TEMP B-TREE FOR ORDER BY` materialised
//! the full fan-out before Rust's dedup-by-first-seen ever saw a row.
//! The fix bounds the temp B-tree fill at `k * SYMBOL_LANE_OVERSAMPLE`
//! rows, leaving Rust's dedup enough candidates to pick the same
//! top-k from.
//!
//! Run:
//!
//! ```sh
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored symbol_bm25_fan_out --nocapture
//! ```
//!
//! Output: a single `perf::symbol_bm25_fan_out` line on stderr with
//! file count, hunks-per-file, total fan-out, k, p50_us, p95_us. Paste
//! it into PR descriptions for any change that touches the lane.

use ohara_core::storage::{CommitRecord, HunkRecord, Storage};
use ohara_core::types::{ChangeKind, CommitMeta, Hunk, RepoId, Symbol, SymbolKind};
use ohara_storage::SqliteStorage;
use std::time::Instant;

/// Number of files the matched symbol lives in. Each file gets its own
/// `Symbol` row (so the FTS5 query matches `FILES` rows). Sized to the
/// "100 files" upper bound called out in issue #57.
const FILES: usize = 100;

/// Hunks per file. The lane's JOIN explodes one symbol-row into one
/// row per (file's hunks) -- so total fan-out before any LIMIT is
/// `FILES * HUNKS_PER_FILE`. Sized to issue #57's "50 hunks each =
/// 5000-row fan-out" target.
const HUNKS_PER_FILE: usize = 50;

/// Iterations to time. Small because the per-call wall-time on the
/// post-fix path is ~hundreds of microseconds; we just want a stable
/// p50/p95 without making the harness slow.
const ITERATIONS: usize = 200;

/// Top-k requested from the lane. Matches `RankingWeights::lane_top_k`
/// in the production retriever (current default = 50; this harness
/// pins 50 explicitly so the timing is comparable across runs).
const TOP_K: u8 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf microbench — opt in via --ignored symbol_bm25_fan_out --nocapture"]
async fn symbol_bm25_fan_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = SqliteStorage::open(dir.path().join("perf.sqlite"))
        .await
        .expect("open SqliteStorage");
    let repo_id = RepoId::from_parts("perf", "/perf/repo");
    storage
        .open_repo(&repo_id, "/perf/repo", "perf")
        .await
        .expect("open_repo");

    // One commit per file -- enough to stitch hunks to a commit_record
    // row without having to touch the parallel-indexer machinery. Each
    // file is then padded out to HUNKS_PER_FILE hunks under that
    // commit so the fan-out matches the issue's target.
    for file_index in 0..FILES {
        let sha = format!("c{file_index:04}");
        let meta = CommitMeta {
            commit_sha: sha.clone(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000_i64 + file_index as i64,
            message: format!("touch hot_{file_index}.rs"),
        };
        storage
            .put_commit(
                &repo_id,
                &CommitRecord {
                    meta,
                    message_emb: vec![0.0_f32; 384],
                    ulid: String::new(),
                },
            )
            .await
            .expect("put_commit");

        let file_path = format!("src/hot_{file_index}.rs");
        let hunks: Vec<HunkRecord> = (0..HUNKS_PER_FILE)
            .map(|hunk_index| {
                HunkRecord::legacy(
                    Hunk {
                        commit_sha: sha.clone(),
                        file_path: file_path.clone(),
                        language: Some("rust".into()),
                        change_kind: ChangeKind::Modified,
                        diff_text: format!("+    line_{hunk_index}();\n"),
                    },
                    vec![0.0_f32; 384],
                )
            })
            .collect();
        storage
            .put_hunks(&repo_id, &hunks)
            .await
            .expect("put_hunks");
    }

    // One head-symbol per file, all sharing the same name. Each
    // symbol contributes one row to fts_symbol_name -- query for
    // `hot_symbol` and SQLite returns FILES symbol rows, each of
    // which the JOIN explodes into HUNKS_PER_FILE hunks.
    let symbols: Vec<Symbol> = (0..FILES)
        .map(|file_index| Symbol {
            file_path: format!("src/hot_{file_index}.rs"),
            language: "rust".into(),
            kind: SymbolKind::Function,
            name: "hot_symbol".into(),
            qualified_name: None,
            sibling_names: Vec::new(),
            span_start: 0,
            span_end: 20,
            blob_sha: format!("blob-{file_index}"),
            source_text: format!("fn hot_symbol() {{ /* file {file_index} */ }}"),
        })
        .collect();
    storage
        .put_head_symbols(&repo_id, &symbols)
        .await
        .expect("put_head_symbols");

    // Warmup: prime SQLite's page cache + statement cache so the
    // first iteration doesn't dominate the percentiles.
    for _ in 0..5 {
        let _ = storage
            .bm25_hunks_by_symbol_name(&repo_id, "hot_symbol", TOP_K, None, None)
            .await
            .expect("bm25 warmup");
    }

    let mut samples_us: Vec<u128> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let hits = storage
            .bm25_hunks_by_symbol_name(&repo_id, "hot_symbol", TOP_K, None, None)
            .await
            .expect("bm25 query");
        let elapsed = start.elapsed();
        samples_us.push(elapsed.as_micros());
        // Sanity: the lane should always return at least one row for
        // a query that matches every file's symbol.
        assert!(
            !hits.is_empty(),
            "lane returned no rows for matching symbol"
        );
        assert!(
            hits.len() <= TOP_K as usize,
            "lane must respect k ceiling: got {} hits for k={TOP_K}",
            hits.len()
        );
    }

    samples_us.sort_unstable();
    let p50_us = samples_us[samples_us.len() / 2];
    let p95_us = samples_us[(samples_us.len() * 95) / 100];
    let total_fan_out = FILES * HUNKS_PER_FILE;

    eprintln!(
        "perf::symbol_bm25_fan_out files={FILES} hunks_per_file={HUNKS_PER_FILE} \
         total_fan_out={total_fan_out} k={TOP_K} iterations={ITERATIONS} \
         p50_us={p50_us} p95_us={p95_us}"
    );
}
