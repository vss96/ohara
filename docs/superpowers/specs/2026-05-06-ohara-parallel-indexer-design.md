# ohara — Parallel commit pipeline (Spec D, plan-28)

**Status:** RFC — ready to plan.

**Companion to (both landed):**
- `2026-05-05-ohara-plan-and-ignore-design.md` (Spec A — plan-26).
- `2026-05-06-ohara-chunk-embed-cache-design.md` (Spec B — plan-27).

## Goal

Process commits concurrently via an actor-style pipeline so cold passes
on giant repos use all available CPU cores instead of one. Combined with
plan-27's chunk-embed cache, parse becomes the dominant per-commit cost
on cache-heavy passes; N workers give close to N× parse parallelism.

```
ohara index --workers <N>     # default: num_cpus::get()
```

## Why this matters

Today `ohara index` processes one commit at a time. With plan-27 turning
most embed calls into cache lookups, the per-commit cost on a warm cache
is ~38 ms (parse 30 ms + cache lookup 5 ms + persist 3 ms). On Linux's
1.44M commits with a fully-warm cache that's still ~15 hours of single-
core work. With 8 workers fanning out the parse stage, the same pass
fits in ~2 hours.

Cold-cache speedup is more modest (~1.5× — embedder throughput is
bounded by accelerator hardware, not worker count) but parse-overlap
still wins. The plan is shaped around the cache-heavy regime because
that's the one users care about for re-indexing.

## Shape

### Actor topology

Three actor types, all `tokio::task` instances, communicating via
bounded `tokio::sync::mpsc` channels:

1. **Walker (1 task).** Walks HEAD-reachable commits via libgit2
   `revwalk` (no topo-order requirement; ULID encodes order). For each
   commit: call `Storage::commit_exists(sha)` (plan-9), drop already-
   indexed commits, compute the ULID, and emit `(CommitMeta, Ulid)` to
   the work channel.

2. **Worker pool (N=`--workers`, default `num_cpus::get()`).** Each
   worker receives one `(CommitMeta, Ulid)` and runs the full per-
   commit pipeline end-to-end: `hunk_chunk → attribute → embed (with
   plan-27 chunk cache) → persist`. One commit = one worker's atomic
   unit. No mid-commit handoff between workers.

3. **No persist serializer.** Workers persist directly. Concurrent
   `put_commit` / `put_hunks` calls serialize at the SQLite WAL writer
   (single-writer-at-a-time), which is fine for ohara's write rate
   (~3 ms each).

**Channel sizing.** `mpsc::channel(N)` between walker and workers.
Walker blocks (backpressure) when the queue is full; workers drain it
as they finish. Memory bounded by N in-flight commits.

**Walker exit.** When the walker finishes enumerating and drops its
sender, workers receive `None` from the closed channel after draining,
finish their current commit, and exit. The coordinator awaits all
worker `JoinHandle`s before returning.

### ULID per commit

```rust
use ulid::Ulid;

/// Derive a ULID from a commit's `(commit_time, commit_sha)`.
/// Deterministic — same input always produces the same ULID.
/// Lexicographic sort = chronological sort.
pub fn ulid_for_commit(commit_time_seconds: i64, sha: &str) -> Ulid {
    let ms = (commit_time_seconds.max(0) as u64).saturating_mul(1000);
    // First 20 hex chars (10 bytes, 80 bits) of the SHA fill the
    // ULID's randomness slot. Two commits at the same millisecond
    // with the same 20-hex-char prefix would collide; in practice
    // SHA-1 prefixes that long are unique.
    let mut rand_bytes = [0u8; 10];
    hex::decode_to_slice(&sha[..20], &mut rand_bytes)
        .expect("invariant: commit_sha is 40-hex");
    let rand_u128 = u128::from_be_bytes({
        let mut buf = [0u8; 16];
        buf[6..].copy_from_slice(&rand_bytes);
        buf
    });
    Ulid::from_parts(ms, rand_u128)
}
```

Properties:
- **Deterministic** — same `(time, sha)` ⇒ same ULID.
- **Time-sortable** — `ORDER BY ulid` returns commits in commit-time
  order.
- **Unique in practice** — collision requires two commits at the same
  ms with identical 20-hex-char SHA prefixes.

### Storage change

`crates/ohara-storage/migrations/V6__commit_ulid.sql`:

```sql
ALTER TABLE commit ADD COLUMN ulid TEXT NOT NULL DEFAULT '';
CREATE INDEX idx_commit_ulid ON commit (ulid);
```

The empty-string default for pre-V6 rows is intentional: existing
indexes still work for retrieval (which doesn't care about ULID
order); ULID-ordered reads (`ORDER BY ulid DESC LIMIT 1`) just skip
them. A `--rebuild` repopulates ULIDs.

`Storage::put_commit` is extended to accept a ULID alongside the
existing `CommitRecord`. The worker computes the ULID and passes it
through.

### `last_indexed_commit` derivation

The stored `repo.last_indexed_commit` column stays as a hot-path cache
updated by each worker as it persists (last writer wins; concurrent
writes are fine since SQLite serializes them).

The authoritative source becomes a query:

```sql
SELECT commit_sha FROM commit ORDER BY ulid DESC LIMIT 1
```

`ohara status` runs this query for the displayed `last_indexed_commit`
to decouple it from worker-write race conditions.

### CLI surface

`ohara index [PATH] --workers <N>` — defaults to `num_cpus::get()`.
`N=1` produces today's serial behavior (the actor topology runs with
a single worker; the channel is just a 1-deep handoff).

`ohara status` prints one new line:

```
workers: 8 (active)         # only when an index pass is running
```

(Optional — the line is informational, not required for correctness.
Out of scope if it complicates the status path.)

### Failure semantics

A worker that errors on one commit logs at warn-level and moves on to
the next message. Other workers and the walker are unaffected. The
failed commit stays unindexed; next run retries. This is a deliberate
downgrade from today's behavior (an error in `run_commit_timed`
propagates up and aborts the whole run) to support long-running
parallel passes where one bad commit shouldn't waste the rest.

The error is recorded once per pass via a tracing event at the end of
the indexer run:

```
indexer: 1,439,824 commits indexed, 12 commits skipped due to errors (see warn logs)
```

Worker-internal panics (not error returns) DO abort the whole run —
panics indicate broken invariants and should not be silently swallowed.

## Constraints

- **No new SQL outside `ohara-storage`.**
- **No `unwrap()` / `expect()` in non-test code** (the `expect("invariant:
  commit_sha is 40-hex")` form is allowed).
- **Walker drops topo-order requirement.** ULID + per-commit
  `commit_exists` is the entire resume mechanism.
- **Backwards compatible.** `--workers 1` is functionally identical to
  today's serial path. Default `--workers num_cpus::get()` requires no
  user action.
- **One model, many workers.** All workers share `Arc<dyn EmbeddingProvider>`.
  The provider must be `Send + Sync` (already required by the trait).
  `embed_batch` calls from N workers serialize through whatever
  threading the provider uses internally (CPU ONNX, CoreML, CUDA);
  spec-D doesn't add a worker-side embed pool.

## Non-goals

- **Multi-process parallelism.** Out of scope.
- **Rayon-based pools.** The storage layer is `async`; rayon doesn't
  compose. Tokio tasks throughout.
- **Topology-aware scheduling.** All workers are equivalent.
- **Cross-repo concurrency.** Each `ohara index` invocation handles one
  repo.
- **Backfill of ULIDs on existing rows during the migration.** A
  follow-up `ohara reindex --backfill-ulid` could populate empty ULIDs
  on existing rows; not needed for this spec because retrieval doesn't
  use ULID and ULID-ordered queries silently skip empty rows.

## Success criteria

- Unit: `ulid_for_commit` deterministic + lex-sortable + unique on
  fixture commits.
- Unit: actor pipeline with a fake `CommitSource`, `ZeroEmbedder`, and
  in-memory storage — N commits through 4 workers produce N persisted
  rows in any order, all `commit_exists(sha) == true` after the run.
- Unit: per-commit failure isolation — one poisoned commit causes a
  warn log; the other 9 of 10 commits persist.
- Integration: `ohara index --workers 4` on a 50-commit fixture
  populates all 50 commits + `SELECT commit_sha FROM commit ORDER BY
  ulid DESC LIMIT 1` matches HEAD.
- Integration: `ohara index --workers 1` produces the same row counts
  + identical `vec_hunk` content as today's serial pass on the same
  fixture (regression).
- Perf: `tests/perf/parallel_indexer_sweep.rs` runs the same fixture
  with `--workers={1,2,4,8}` and prints wall-time per pass. Operator-
  run; not in CI.

## Out of scope (deferred)

- ULID backfill for pre-V6 rows.
- `ohara status workers:` line if it complicates the status path.
- Per-stage worker tuning (`--parse-workers` / `--embed-workers`).
- Embed batch pooling across workers (deferred until measurement
  shows an accelerator pipeline benefit).

## Related

- `Cargo.toml` — add `ulid = "1"` to `[workspace.dependencies]`,
  `num_cpus = "1"` if not already present.
- `crates/ohara-core/Cargo.toml` — reference the new deps.
- `crates/ohara-storage/migrations/V6__commit_ulid.sql` — new migration.
- `crates/ohara-storage/src/tables/commit.rs` — `put_commit` accepts
  a ULID; `SELECT … ORDER BY ulid` helper for `ohara status`.
- `crates/ohara-core/src/types.rs` — `pub fn ulid_for_commit(time, sha)`.
- `crates/ohara-core/src/storage.rs` — extend `Storage::put_commit`
  signature with a ULID param.
- `crates/ohara-core/src/indexer.rs` — `Indexer::with_workers(n)`
  builder; thread to the new actor coordinator.
- `crates/ohara-core/src/indexer/coordinator/mod.rs` — replace the
  per-commit `for` loop with the actor topology (walker task +
  worker tasks + bounded channel). Today's serial path becomes
  `--workers 1`.
- `crates/ohara-cli/src/commands/index.rs` — `--workers` clap arg.
- `crates/ohara-cli/src/commands/status.rs` — derived
  `last_indexed_commit` from `MAX(ulid)`.
- `tests/perf/parallel_indexer_sweep.rs` — operator perf harness.
- Prior art: plan-9 (commit-exists skip), plan-19 (5-stage pipeline),
  plan-26 (Spec A), plan-27 (Spec B).
