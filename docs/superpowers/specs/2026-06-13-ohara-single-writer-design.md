# Single-writer storage pool (plan-32)

**Date:** 2026-06-13
**Status:** implemented
**Driver:** Even after plan-31 raised `busy_timeout` to 30s, a questdb CoreML
rebuild still dropped 4 of 6,004 commits with `database is locked`, and the
storage phase ballooned to 458% (7,020s — up from 88% pre-CoreML). Both are the
same root cause: SQLite (WAL) allows exactly one writer, and the plan-28
parallel indexer's ~`num_cpus` workers each open their own pooled write
connection. They fight SQLite's busy handler — the losers spin in its
poll-sleep loop (counted as `storage_write_ms`, hence the 458%), and the deepest
tail occasionally exceeds even 30s and drops the commit. CoreML-fast embedding
makes it worse by clustering the writes.

## Design

The textbook SQLite concurrency model: **one writer, many readers.**

`SqliteStorage` opens two pools over the same DB file:
- `pool` — the existing read pool (deadpool default `num_cpus * 4`), used by all
  reads (KNN, BM25, `get_*`, `commit_exists`, cache lookups, status/metadata
  reads). Concurrent reads are safe in WAL.
- `write_pool` — a new `SqlitePoolBuilder::max_size(1)` pool. Every index write
  (`put_commit`, `put_hunks`, `put_head_symbols`, `clear_head_symbols`,
  `set_last_indexed_commit`, `put_index_metadata`, `record_blob_seen`,
  `embed_cache_put_many`, `open_repo`) routes here. With one connection, deadpool
  serializes callers FIFO on the app side — no contention on SQLite's busy
  handler, no poll-sleep waste, and no timeout-driven drops.

`busy_timeout` (30s, plan-31) stays as a backstop for the *cross-process* case
(a daemon reading/writing the same index while `ohara index` runs); intra-process
worker contention is now eliminated structurally.

This is not rayon (which is CPU-bound data parallelism — the opposite of what a
single writer needs). The storage layer is async: deadpool runs the blocking
`rusqlite` calls on a thread pool via `interact`, and the single-writer is a
tokio-level serialization (the max-size-1 pool's semaphore).

## Validation

Medium fixture (2,045 commits) rebuilt via CoreML: **0 commits dropped**
(exit 0), storage phase **39%** (51s) — vs the 458% busy-wait inflation on the
buggy run. Unit test drives 16 concurrent `put_commit` calls and asserts all
succeed through a `max_size == 1` write pool.

## Out of scope

- A dedicated writer *task* (vs the max-size-1 pool) — the pool's semaphore
  already gives FIFO serialization with less code.
- Cross-process write coordination beyond `busy_timeout`.
