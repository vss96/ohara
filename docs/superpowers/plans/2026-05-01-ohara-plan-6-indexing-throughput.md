# ohara v0.6 — Indexing throughput implementation plan

> **For agentic workers:** TDD red/green per task. Standards match
> Plans 1–5 (no commit attribution; workspace-green at every commit;
> cargo fmt + clippy + test clean at end). v0.5.1 is the floor —
> abort-resume safety, ProgressSink, and `--commit-batch` /
> `--threads` / `--no-progress` are already shipped, don't re-do.

**RFC:** `docs/superpowers/specs/2026-05-01-ohara-v0.6-indexing-throughput-rfc.md`.
The success criteria there are the contract: hit either (A) <15 min
total or (B) <3 min to useful partial index on the QuestDB-class
reference repo.

**Phasing:** v0.6 is **two phases**, gated on profile data.

- **Phase 1 (Task 0–2):** measure. Land a profile harness, capture
  baseline numbers on a real fixture, ship a CI perf-regression
  test infrastructure. **No architectural changes in Phase 1.**
- **Phase 2 (Tasks 3+):** implement. Specific architectural choices
  are decided by Phase 1's numbers. The plan below lists the most
  likely interventions in priority order; the implementer reorders
  / drops / adds based on what the profile says.

The implementer should commit Phase 1 in full before opening any
Phase 2 task. If Phase 1's profile flips the priority order or
reveals a bottleneck not listed below, **edit this plan in place
and re-commit it** rather than executing on stale assumptions.

---

## Phase 1 — Measure

### Task 0: Profile harness + baseline numbers

- [ ] **0.1: Per-phase wall-time instrumentation in `Indexer::run`.**
      Add a `PhaseTimings` struct:
      ```rust
      pub struct PhaseTimings {
          pub commit_walk_ms: u64,
          pub diff_extract_ms: u64,
          pub tree_sitter_parse_ms: u64,
          pub embed_ms: u64,
          pub storage_write_ms: u64,
          pub fts_insert_ms: u64,
          pub head_symbols_ms: u64,
      }
      ```
      Use `Instant::now()` around each phase; sum into the struct;
      attach to `IndexerReport` (additive; existing fields stay).
      Must compile clean even when phases are interleaved (e.g. the
      embed call sits inside the commit-walk loop today). When in
      doubt, measure the inner phase as cumulative wall-time across
      all loop iterations.
      Tests: a unit test that runs the indexer with a `FakeStorage`
      + `FakeEmbedder` + `FakeCommitSource` and asserts each
      `PhaseTimings` field is populated (>0).

- [ ] **0.2: `ohara index --profile` flag.** When set, after the run,
      emit `PhaseTimings` as JSON to stdout (alongside the existing
      summary line). Suitable for piping into `jq` or pasting into
      `docs/perf/v0.6-baseline.md`. Test that the JSON parses and
      contains all expected keys.

- [ ] **0.3: Hunk-text inflation measurement.** Add to `PhaseTimings`:
      ```rust
      pub total_diff_bytes: u64,
      pub total_added_lines: u64,
      ```
      so `bytes_per_added_line = total_diff_bytes / total_added_lines`
      can be computed at the call site. The cheap-win measurement
      from the RFC. Default git2 context is 3 lines; a high ratio
      (>4×) signals trimmable input.

- [ ] **0.4: Fixture for the perf benchmark.** Build script at
      `tests/perf/fetch_questdb.sh` that shallow-clones QuestDB at a
      pinned SHA into `target/perf-fixtures/questdb/` if not present.
      Vendoring a 200MB+ pack is too expensive; building on demand
      keeps it out of the source tarball.

- [ ] **0.5: Run profile, paste results.** Run `ohara index --profile
      --no-progress` against the QuestDB fixture. Paste the JSON
      output (per phase, in ms) and the hunk-inflation ratio into
      a new file `docs/perf/v0.6-baseline.md`. This is the single
      source of truth for Phase 2 prioritization decisions.

### Task 1: CI perf-regression infrastructure

- [ ] **1.1: `tests/perf/quest_db_baseline.rs`** — `#[ignore]`'d
      e2e that:
      1. Runs `tests/perf/fetch_questdb.sh` to populate the fixture.
      2. Calls `ohara_cli::commands::index::run(...)` with
         `--no-progress`, capturing the `IndexerReport` +
         `PhaseTimings`.
      3. Asserts a wall-time threshold (e.g.
         `total_ms < BASELINE_MS * 1.10` to catch 10% regressions).
         The exact number comes from Task 0.5.
      4. Asserts the v0.3 retry-pattern e2e passes against the
         indexed result (quality gate from RFC constraint #7).

- [ ] **1.2: Scheduled CI workflow.** Add
      `.github/workflows/perf.yml` that runs the perf test weekly
      (cron) and on demand (workflow_dispatch). Fails the workflow
      if the wall-time threshold is missed; posts the JSON
      breakdown as a comment / annotation. **Don't gate the
      release workflow on this** — perf is informational, not
      blocking, until Phase 2 ships and we have a real budget.

### Task 2: Quality-gate e2e for any new perf flag

- [ ] **2.1: Helper test pattern.** Document (in
      `tests/perf/README.md`) the pattern any Phase 2 perf flag
      must follow: a paired `#[ignore]`'d e2e that runs the same
      query (e.g. retry-pattern) with the flag on AND off, and
      asserts the rank-1 result is identical. The flag-on path
      may be faster; it must not lose retrieval quality on the
      reference fixture.
      No code in this task — just the README + an example
      paired-test skeleton in `tests/perf/example_paired.rs`
      (`#[ignore]`'d, behind a `#[cfg(any())]` gate so it
      doesn't compile until a real flag uses it).

---

## Phase 2 — Implement (provisional, profile-driven)

The order below is the **most likely** ranking based on the RFC's
analysis. Phase 1's profile numbers should reorder this. Do not
execute Phase 2 before Phase 1 lands.

### Task 3: ONNX execution provider flag (`--embed-provider`)

Most likely highest-yield, smallest change. Land this first in Phase 2
unless Task 0 says embed is NOT the bottleneck.

- [ ] **3.1: Plumb provider config through `FastEmbedProvider::new`.**
      Today the provider takes no args. Change to
      `FastEmbedProvider::with_provider(EmbedProvider) -> Result<Self>`,
      where `EmbedProvider` is an enum: `Cpu` (default), `CoreMl`,
      `Cuda`. Use fastembed's `RuntimeBuilder`-style API (verify
      against fastembed v4.9 source first; if the surface isn't
      exposed, drop to `ort` directly via the `ort` crate).
- [ ] **3.2: CLI flag + auto-detect.** Add `--embed-provider <auto |
      cpu | coreml | cuda>` to `commands::index::Args`. In `auto`
      (default), detect the platform: macOS arm64 → CoreML; Linux
      with `CUDA_VISIBLE_DEVICES` set → CUDA; else CPU. Surface
      the chosen provider in `tracing::info!`.
- [ ] **3.3: Paired e2e (per Task 2.1).** Run retry-pattern with
      `--embed-provider cpu` and `--embed-provider auto`, assert
      same rank-1.

### Task 4: Hunk-text trimming

If Task 0.5 shows `bytes_per_added_line > 4`, the embedder is
chewing on context lines that don't carry the change's signal.

- [ ] **4.1: Switch git2 diff context from 3 to 1 line** in
      `crates/ohara-git/src/diff.rs::DiffOptions::context_lines(1)`.
      Quality contract: re-run retry-pattern e2e; rank-1 must hold.
- [ ] **4.2: Re-run profile (Task 0.5).** Compare. If embed time
      drops 30%+ without quality regression, ship the change.
      Otherwise revert and document why in
      `docs/perf/v0.6-baseline.md`.

### Task 5: Pipeline parallelism (walk → parse → embed → write)

If Task 0.5 shows the bottleneck is sequential — i.e. embed and
write are alternating instead of overlapping — wire a producer/
consumer queue between phases.

- [ ] **5.1: Bounded mpsc channel** between commit-walk and embed.
      The walker fills hunks into the queue; the embedder pulls
      batches off. Storage writes happen on a third stage. Tokio
      tasks (not OS threads) — the work inside spawn_blocking.
- [ ] **5.2: Backpressure.** Channel capacity = `--commit-batch *
      avg_hunks_per_commit`. If embed lags, walker blocks rather
      than ballooning RSS.
- [ ] **5.3: Resume safety still holds.** The watermark advance
      semantics from v0.5.1 must survive the rewrite — either
      keep watermark advancement attached to storage-write
      completion (not commit-walk completion), or document the
      new contract explicitly.
- [ ] **5.4: Paired e2e** with the unflagged path.

### Task 6: `--resources auto` mode

Wraps `--commit-batch` + `--threads` + `--embed-provider` into a
single per-host policy. The CLI default becomes `--resources auto`;
explicit flags override.

- [ ] **6.1: Lookup table.** `pick_resources(&Host) ->
      ResourcePlan` based on:
      - cores (logical),
      - RAM (free + total),
      - GPU/NE availability (CoreML / CUDA),
      - estimated repo size (commit count + pack size, if cheaply
        readable from git2).
      Concrete ranges in the lookup table are decided after Task 0.5
      data is in.
- [ ] **6.2: `--resources <auto|conservative|aggressive>`** flag.
      `conservative` halves the auto-picked thread count and batch
      size (good for shared dev boxes). `aggressive` doubles them
      (good for dedicated indexing runs).
- [ ] **6.3: Surface the chosen plan in `tracing::info!`** at
      startup so users see what was picked and why.

### Task 7 (optional, profile-driven): Recency-first partial index

Path (B) from the RFC. Only do this if Task 0.5 + Tasks 3–5
combined don't hit success criterion (A).

- [ ] **7.1: Two-watermark schema.** Today: single `last_indexed_commit`.
      New: `last_indexed_commit` (newest fully indexed) +
      `oldest_indexed_commit` (backfill watermark). Migration V3
      adds `oldest_indexed_commit TEXT` column.
- [ ] **7.2: Walker ordering.** First pass: walk newest N (e.g.
      500) commits, write, set both watermarks. Subsequent
      passes: extend `oldest_indexed_commit` backward in batches.
- [ ] **7.3: `_meta.indexed_window`** in MCP responses — surface
      the [oldest, newest] range so Claude knows what's been
      covered. `find_pattern` results outside the window get a
      `partial_index: true` flag in their `_meta` for transparency.
- [ ] **7.4: CLI override:** `ohara index --recency-first` to
      opt in (or `--full` to opt out, depending on what Task 0
      says is the right default).

### Task 8: Final pass

- [ ] `cargo fmt --all && cargo clippy --workspace --all-targets
      -- -D warnings && cargo test --workspace`. README gets a
      "v0.6 throughput" paragraph linking to
      `docs/perf/v0.6-baseline.md`.

---

## Done when

- Task 0–2 complete (Phase 1 always lands).
- ≥ 1 of (A) <15 min total or (B) <3 min to useful partial index
  is met against the QuestDB fixture per Task 1.1's assertion.
- All paired-e2e quality gates pass (no retry-pattern rank-1
  regression).
- `docs/perf/v0.6-baseline.md` records before / after numbers for
  every Phase 2 task that landed.

## Non-goals (per RFC)

Cloud calls, GPU-required builds, distributed indexing, retrieval
pipeline rework, Windows.

## Risk / fallback

- **Task 0 reveals embed isn't the bottleneck.** Skip Task 3,
  reorder Phase 2 by the profile's actual hot path. Document
  the surprise.
- **Apple Silicon CoreML provider isn't exposed by fastembed v4.9.**
  Drop to `ort` crate directly. Larger surface change but still
  doable in Task 3.
- **Recency-first changes the resume contract** (RFC Open Q #5).
  v0.5.1's per-100-commits watermark assumes linear walk. If
  Task 7 lands, the watermark becomes a `[oldest, newest]` range
  and the resume math gets harder. Document the new contract in
  the PR; add tests for resume-after-abort during backfill.
- **CI perf job is flaky on shared GitHub runners** (CPU contention
  varies). Run the assertion as `total_ms < threshold + std_dev`
  rather than `total_ms < threshold`, and only block on 3
  consecutive regressions, not 1.

## Files affected (rough; will firm up after Task 0)

Phase 1:
- `crates/ohara-core/src/indexer.rs` — `PhaseTimings` + threading
- `crates/ohara-cli/src/commands/index.rs` — `--profile` flag
- `crates/ohara-git/src/diff.rs` — possibly add per-call timer hook
- `tests/perf/fetch_questdb.sh` (new)
- `tests/perf/quest_db_baseline.rs` (new)
- `tests/perf/README.md` (new)
- `tests/perf/example_paired.rs` (new, gated)
- `.github/workflows/perf.yml` (new)
- `docs/perf/v0.6-baseline.md` (new, written by Task 0.5)

Phase 2 (provisional):
- `crates/ohara-embed/src/fastembed.rs` — provider config
- `crates/ohara-cli/src/commands/index.rs` — `--embed-provider`,
  `--resources`
- Possibly `crates/ohara-storage/src/migrations/V3*.sql` if Task 7
  lands.
