//! Per-method query-grain latency counters for the perf harness.
//!
//! Only the **read lanes** (KNN, BM25, get_hunk_symbols, neighbour
//! lookups, status, metadata) are instrumented — write-path methods
//! (`open_repo`, `put_*`, `set_last_indexed_commit`, blob-cache
//! mutators) are left uninstrumented because plan-14 is a
//! query-perf substrate. Phase 2+ work that targets indexing
//! latency will need to backfill the missing surfaces.
//!
//! **Snapshot consistency:** `StorageCounters::snapshot` reads each
//! atomic field independently with `Relaxed` ordering. Under heavy
//! concurrent read traffic the snapshot can be mildly inconsistent —
//! e.g. `call_count` may have advanced past a `total_elapsed_us` read
//! for the same call. This is acceptable for an operator-run perf
//! harness (we're sampling, not auditing) but the snapshot is not
//! "accounting-grade" and shouldn't be treated as such.

use ohara_core::storage::{StorageMethodMetrics, StorageMetricsSnapshot};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
pub(crate) struct MethodCounter {
    call_count: AtomicU64,
    total_elapsed_us: AtomicU64,
    rows_returned: AtomicU64,
}

impl MethodCounter {
    pub(crate) fn record(&self, elapsed_us: u64, rows: u64) {
        self.call_count.fetch_add(1, Relaxed);
        self.total_elapsed_us.fetch_add(elapsed_us, Relaxed);
        self.rows_returned.fetch_add(rows, Relaxed);
    }
    pub(crate) fn snapshot(&self) -> StorageMethodMetrics {
        StorageMethodMetrics {
            call_count: self.call_count.load(Relaxed),
            total_elapsed_us: self.total_elapsed_us.load(Relaxed),
            rows_returned: self.rows_returned.load(Relaxed),
        }
    }
}

#[derive(Default)]
pub(crate) struct StorageCounters {
    pub(crate) knn_hunks: MethodCounter,
    pub(crate) bm25_hunks_by_text: MethodCounter,
    pub(crate) bm25_hunks_by_semantic_text: MethodCounter,
    pub(crate) bm25_hunks_by_symbol_name: MethodCounter,
    pub(crate) bm25_hunks_by_historical_symbol: MethodCounter,
    pub(crate) get_hunk_symbols: MethodCounter,
    pub(crate) get_hunk_symbols_batch: MethodCounter,
    pub(crate) get_neighboring_file_commits: MethodCounter,
    pub(crate) get_index_status: MethodCounter,
    pub(crate) get_index_metadata: MethodCounter,
}

impl StorageCounters {
    pub(crate) fn snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot {
            knn_hunks: self.knn_hunks.snapshot(),
            bm25_hunks_by_text: self.bm25_hunks_by_text.snapshot(),
            bm25_hunks_by_semantic_text: self.bm25_hunks_by_semantic_text.snapshot(),
            bm25_hunks_by_symbol_name: self.bm25_hunks_by_symbol_name.snapshot(),
            bm25_hunks_by_historical_symbol: self.bm25_hunks_by_historical_symbol.snapshot(),
            get_hunk_symbols: self.get_hunk_symbols.snapshot(),
            get_hunk_symbols_batch: self.get_hunk_symbols_batch.snapshot(),
            get_neighboring_file_commits: self.get_neighboring_file_commits.snapshot(),
            get_index_status: self.get_index_status.snapshot(),
            get_index_metadata: self.get_index_metadata.snapshot(),
        }
    }
}

pub(crate) async fn timed_with_conn<F, T>(
    pool: &deadpool_sqlite::Pool,
    counter: &MethodCounter,
    rows_of: impl Fn(&T) -> u64,
    f: F,
) -> ohara_core::Result<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let start = std::time::Instant::now();
    let out = crate::storage_impl::with_conn(pool, f).await?;
    let elapsed_us = start.elapsed().as_micros() as u64;
    let rows = rows_of(&out);
    counter.record(elapsed_us, rows);
    Ok(out)
}
