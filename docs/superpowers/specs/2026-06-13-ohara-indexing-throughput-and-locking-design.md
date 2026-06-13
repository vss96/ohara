# Indexing throughput + write-locking fix (plan-31)

**Date:** 2026-06-13
**Status:** approved (follows the v0.11 CoreML work; addresses two issues found
operating the new path on questdb)
**Driver:** Two defects surfaced indexing questdb (6,004 commits) on the v0.11
CoreML path:

1. **Silent data loss.** The plan-28 parallel indexer runs ~`num_cpus` workers,
   each opening its own SQLite write transaction. The DB is WAL but has **no
   `busy_timeout`**, so concurrent writers get `SQLITE_BUSY` ("database is
   locked") immediately; the worker logs a debug warning and does
   `wr.failed += 1`, dropping the commit. A questdb CoreML run indexed 5,638 of
   6,004 commits — ~366 missing, ~340 more than the CPU baseline. Faster
   (CoreML) embedding makes it worse: workers finish embedding in a tight
   cluster and pile onto the single writer at once.
2. **CoreML padding waste.** `CoreMlFixedProvider` embeds in fixed 32-row
   batches and pads partial batches with empty strings. Embedding runs
   **per-commit** (`EmbedStage::run` once per commit), and 64% of questdb
   commits have ≤8 rows (msg + hunks). Measured: 116,192 rows needed but
   259,104 embedded — **55.2% wasted**. This is the gap between the ~20 min the
   harness projected and the ~40 min observed.

## Part 1 — Write locking

### `apply_pragmas` (ohara-storage)

**Investigation correction:** rusqlite already sets `sqlite3_busy_timeout(db,
5000)` on every connection (`inner_connection.rs`), and both indexer write paths
(`commit::put`, `hunk::put_many`) are write-first (`DELETE` then `INSERT`), so
this is *not* a read→write-upgrade deadlock. The drops come from **tail-worker
starvation**: under CoreML-fast embedding the ~`num_cpus` workers finish
embedding in a tight cluster and queue on the single WAL writer; each commit's
write transaction (including vec0 inserts) takes hundreds of ms, so workers at
the back of a clustered wave exceed the 5s default and drop their commit.

Fix: raise the timeout above the worst-case queue wait by adding
`PRAGMA busy_timeout=30000;` to the per-connection pragma batch in
`crates/ohara-storage/src/codec/pool.rs` (overriding rusqlite's 5000ms). With
~12 workers each holding the lock <1s, the deepest queue wait is ~11s, well
under 30s. The pragma's busy handler then serializes writers instead of dropping
commits; if 30s is ever exceeded, the failure is now visible (below), not
silent.

### Surface dropped commits (ohara-core)

Silent `wr.failed += 1` becomes visible:

- `IndexerReport` gains `pub commits_failed: usize` (cumulative across the run).
- The coordinator threads the per-worker `failed` count into the report
  (it already sums `commits_failed` in `ActorResult`; plumb it through to
  `IndexerReport`).
- `ohara index`'s human summary prints a line when `commits_failed > 0`
  (e.g. `⚠ 3 commits failed to index (see warnings above)`), and the command
  exits non-zero so scripts and the post-commit hook notice.
- Worker warnings stay, but a hard write failure is no longer invisible at the
  top level.

### Test plan (Part 1)

- ohara-storage: a test that opens two pooled connections and runs overlapping
  write transactions; with `busy_timeout` set, both commit (no `SQLITE_BUSY`).
  Pin it deterministic by holding one write txn open briefly while the second
  starts.
- ohara-core: `IndexerReport.commits_failed` is populated from the actor result
  (unit test on the coordinator merge with a storage stub that fails one
  commit).
- ohara-cli: summary renderer shows the failed-commits line when count > 0 and
  omits it at 0.

## Part 2 — Cross-commit batching coalescer

### `BatchingEmbedder` (ohara-embed)

A decorator implementing `EmbeddingProvider` that wraps any inner
`Arc<dyn EmbeddingProvider>` and coalesces rows across calls into full batches:

- Construction spawns one **coalescer task** owning the inner embedder and an
  `mpsc` receiver of `Request { texts: Vec<String>, reply: oneshot::Sender<Result<Vec<Vec<f32>>>> }`.
- `embed_batch(texts)` sends a `Request` and awaits its `reply` — so the
  decorator honours the existing contract (output length == input length, same
  order) while the *physical* ONNX batch is filled across callers.
- The coalescer loop:
  1. Block on the first request; buffer its rows, tracking
     `(reply, start, len)` spans.
  2. **Greedily drain** any immediately-available requests (`try_recv`) into the
     buffer.
  3. While the buffer holds ≥ `batch_rows`, dispatch a full `batch_rows` slice
     to the inner `embed_batch`, then fan results back to every span fully
     covered by that slice (a span straddling a batch boundary is completed once
     its later rows are dispatched in the next slice).
  4. When no more requests are immediately available, **flush the partial
     remainder** (one final inner call) so the tail never stalls, then loop back
     to the blocking receive.
- `batch_rows` defaults to `CoreMlFixedProvider::FIXED_BATCH` (32) so the inner
  CoreML provider receives exactly one un-padded batch per dispatch.
- Inner-call errors propagate to every span in that dispatch via their `reply`.
- On `Drop` / sender-closed, the task flushes and exits.

`dimension()` / `model_id()` delegate to the inner embedder.

### Wiring (ohara-cli / ohara-core)

Wrap the embedder in `BatchingEmbedder` on the CoreML path only (it is
semantically transparent — sentence embeddings have no cross-row attention — so
correctness is unaffected, but CPU sees no padding penalty and gains nothing, so
we avoid changing that path's behaviour). The coordinator/`EmbedStage` are
unchanged: they still call `embed_batch` per commit; the coalescer does the
cross-commit work beneath them. Per-commit persist and resume-safety are
untouched — each commit still persists atomically after *its* rows return.

### Test plan (Part 2)

- Coalescer unit tests against a deterministic stub inner embedder that returns
  a vector encoding each input's identity:
  - N concurrent `embed_batch` calls each get back exactly their own rows, in
    order (correct scatter across batch boundaries).
  - A single call larger than `batch_rows` is split and reassembled correctly.
  - A single small call still returns (flush-on-drain; no hang).
  - Inner-embedder error propagates to all in-flight callers of that dispatch.
  - Instrumented stub asserts the inner embedder receives full `batch_rows`
    batches under load (the padding-elimination property).
- Harness/measurement: extend or add a perf cell showing dispatched-row count ==
  needed-row count (no padding) for a questdb-shaped per-commit workload.

## Out of scope (deferred)

- Full streaming rearchitecture (single embed task + single writer task fed by
  parallel diff/parse workers). `busy_timeout` removes the correctness issue and
  the coalescer captures the throughput, so the larger refactor is a future
  option, not now.
- Retrying individual `SQLITE_BUSY` writes in application code (the pragma's
  busy handler covers it).
- Tuning `--commit-batch` / WAL autocheckpoint (single-digit-% follow-ups).

## Expected impact

Eliminating the 55.2% padding waste roughly halves the embed phase (the
dominant cost), targeting the harness's ~20 min projection on questdb; the
locking fix restores full commit coverage (6,004 vs the dropped 5,638).
