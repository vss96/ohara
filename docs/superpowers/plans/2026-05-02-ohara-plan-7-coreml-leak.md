# ohara v0.6.1 — CoreML memory leak fix plan

> **Status: Path B taken (2026-05-02).** Phase 1 reproduced the leak
> in a tight `embed_batch` loop on a macOS-26.2 / arm64 / 24 GB host;
> the leak is heap-attributable (~4 MB/batch, `MALLOC_LARGE`), and the
> rebuild-cadence probe at K=50 mitigates by ~2× but does not bound
> growth. v0.6.1 ships the documented workaround. Diagnosis lives in
> `docs/perf/v0.6.1-leak-diagnosis.md`. Phase 2A is parked pending an
> upstream fix in fastembed / ort.

> **For agentic workers:** investigation-driven plan. Tasks 1–2 are
> diagnosis (no commits to production code yet — only the harness and
> the diagnosis doc, both explicitly approved by the RFC's
> Investigation framework). Task 3 onward depends on what the
> diagnosis says. Update this file in place after each phase.

**RFC:** `docs/superpowers/specs/2026-05-02-ohara-v0.6.1-coreml-leak-rfc.md`.
The success criteria there are the contract. (A) is the preferred
landing; (B) is the documented-workaround fallback.

**Path A vs Path B is decided by whether a rebuild-cadence in our
wrapper holds peak `phys_footprint` below 4 GB across the harness — not
by where the root cause lives.** Even if the underlying bug is in
`CoreML.framework` (RFC hypothesis 3), the in-tree mitigation is the
same: rebuild the `TextEmbedding` periodically. Path B is reserved for
the case where rebuild-cadence cannot bound footprint, or where it does
but introduces unacceptable wallclock overhead or rank-quality
regressions.

**Phase 1 time-box:** if Task 2.2 has not produced a Path A or Path B
verdict by **2026-05-09**, declare Path B and proceed.

## Phase 1 — Diagnose

### Task 1: Minimal reproduction harness

- [ ] **1.1: Carve out the smallest workload that reproduces the
      leak.** Goal: a script that runs in 5–10 minutes, hits the same
      30+ GB compressed-memory shape, and doesn't require QuestDB.
      - Shape: a tight Rust loop calling
        `FastEmbedProvider::embed_batch(&[text; B])` for `N`
        iterations against `EmbedProvider::CoreMl`. Pick `B` and
        text-length to mirror the indexer's real workload — diff
        chunks are typically 100–300 tokens at batch size 8–32.
        Start with `B = 16`, ~200-token strings, and tune `N`
        upward until the harness either OOMs or plateaus.
      - **Memory metric: `phys_footprint` and `compressed`, not
        RSS.** macOS jetsam killed at 32 GB *compressed*, not RSS.
        Sample via `proc_pid_rusage(getpid(), RUSAGE_INFO_V4, ...)`
        → `ri_phys_footprint` once per iteration, or shell out to
        `footprint -p <pid>` if calling rusage is too much
        ceremony. RSS will plateau in the low GBs while the actual
        kill metric keeps climbing.
      - Land the harness as `tests/perf/coreml_leak_repro.rs`,
        `#[ignore]`'d like the rest of `tests/perf/`. Stays as a
        regression guard regardless of which path lands.
      - If the in-process loop does **not** reproduce in 10
        minutes, escalate to a full `ohara index` pass against a
        synthetic fixture. **Note:** `fixtures/build_synthetic.sh`
        does not exist today; if needed, create it as part of this
        task and call out in the PR.

- [ ] **1.2: Capture the allocation profile.** Tools, in order of
      relevance to *this* leak:
      1. `footprint -p <pid>` sampled every 5 s — gives `phys_
         footprint`, `dirty`, `swapped`, `compressed`, broken down by
         region. **First tool to run; it's the only one that maps
         directly to the kill metric.**
      2. `vmmap --summary <pid>` — region-level breakdown. Look for
         growth in `MALLOC_TINY`, `MALLOC_SMALL`, and any region
         tagged `IOAccelerator` / `CoreML` / `ANE`.
      3. `heap <pid>` — process-wide malloc summary. Use to confirm
         whether growth is in heap (our or fastembed's) vs outside
         heap (kernel-side ANE retention).
      4. `instruments -t Allocations` — only if heap growth is
         confirmed by step 3. Does **not** see ANE-side memory or
         kernel-attributed wired bytes; if hypothesis (3) is right,
         this tool returns clean and is misleading.
      5. `leaks --atExit` — only catches *unreachable* allocations.
         Won't see "freed late" patterns; document negative result
         and move on.
      - Save the trace summaries + a one-paragraph narrative to
        `docs/perf/v0.6.1-leak-diagnosis.md` (creation explicitly
        approved by RFC §Investigation framework). Reference which
        of the RFC's hypotheses the data supports, and which it
        rules out.

- [ ] **1.3: Pin the leak source.** From the profile, identify *the*
      region or call site responsible for unbounded growth. Map back
      to RFC hypotheses:
      - Growth in heap, attributed to ort symbols → hypothesis (1)
        or (4); fix is rebuild-cadence in our wrapper.
      - Growth in heap, attributed to fastembed / `TextEmbedding`
        symbols → hypothesis (2); same fix.
      - Growth outside heap (`IOAccelerator` / `ANE` regions, wired
        memory) → hypothesis (3); rebuild-cadence may still help if
        the kernel releases on session disposal.
      - Growth in our own code paths → hypothesis (5); fix obvious.

- [ ] **1.4: Disambiguate growth axis.** Run three short variants of
      the harness:
      - `f(batches)`: hold wallclock-per-iter constant, vary batch
        count.
      - `f(wallclock)`: hold batch count constant, add a sleep
        between batches.
      - `f(ANE-on-time)`: hold both constant, force ANE off between
        batches if a probe to do that exists; otherwise note as
        "not directly probeable, infer from above two."
      Outcome dictates the fix:
      - per-batch → rebuild every K batches.
      - per-wallclock → rate-limit batch dispatch (worse).
      - growth survives session rebuild on the same harness run →
        Path B only; rebuild-cadence cannot fix it.

### Task 2: Validate hypothesis

- [ ] **2.1: Probe rebuild-cadence in the harness.** Single concrete
      probe — drop and rebuild `TextEmbedding` every K calls. No
      production change yet.
      - The probe must re-run `apply_provider_to_init`
        (`crates/ohara-embed/src/fastembed.rs:108-115`) so the new
        session keeps CoreML attached. If the probe silently ends
        up CPU-only it invalidates the data.
      - Sweep K ∈ {50, 200, 1000} and record peak `phys_footprint`
        and wallclock per iteration. Plot or tabulate in the
        diagnosis doc.
      - Negative results matter: if growth survives the rebuild
        across all K, document as "rebuild-cadence cannot bound
        footprint" and skip to Path B.

- [ ] **2.2: Decide on the fix shape.** Post a one-paragraph decision
      in the diagnosis doc:
      - **Path A is viable iff** the rebuild-cadence probe holds
        peak `phys_footprint < 4 GB` across the full harness, with
        rebuild overhead < 10% wallclock vs the no-rebuild CPU
        baseline.
      - **Path B otherwise.** Reasons can include: rebuild does not
        bound footprint; rebuild is too expensive; rebuild
        perturbs rank quality past the relaxed gate (see 3.3).
      - Decision is hard-deadlined to **2026-05-09**. If the data
        is inconclusive on that date, declare Path B.

## Phase 2A — In-tree fix (if Path A from Task 2.2)

### Task 3: Implement the fix

- [ ] **3.1: Failing test first.** Extend
      `tests/perf/coreml_leak_repro.rs` to assert peak
      `phys_footprint < 4 GB` across N iterations under CoreML.
      `#[ignore]`'d (CoreML feature required — not all CI hits it).

- [ ] **3.2: Land the fix in `ohara-embed`.** Shape:
      - Add `recreate_after_n_batches: Option<usize>` to
        `FastEmbedProvider`. Default `None` for non-CoreML; set to
        the K from Task 2.1 when CoreML is the provider.
      - Field changes from `model: Arc<Mutex<TextEmbedding>>`
        (`crates/ohara-embed/src/fastembed.rs:40`) to
        `model: Mutex<EmbedderState>` where
        `struct EmbedderState { embedder: TextEmbedding, calls: usize }`.
        Lock held during the swap to avoid a concurrent caller
        seeing a half-rebuilt embedder.
      - Recreate path **must** call `apply_provider_to_init` again
        with the same `EmbedProvider`, otherwise the rebuilt
        session silently downgrades to CPU.
      - Apply the same shape to `FastEmbedReranker` (the reranker
        also holds a CoreML-attached session).

- [ ] **3.3: Quality gate.** The Plan 6 paired e2e
      (`tests/perf/embed_provider_paired.rs`) must still pass.
      Acceptable degradations:
      - **Strict gate** (default): rank-1 retry hit identical CPU
        vs CoreML — keep if achievable.
      - **Relaxed gate** (only if rebuild introduces ANE warm-up
        nondeterminism): top-3 contains the retry hit. Document
        the relaxation in the diagnosis doc and PR description.

- [ ] **3.4: Log the cadence at startup.**
      `tracing::info!(recreate_after_n_batches = ?N, provider = ?p, "embedder")`
      from `FastEmbedProvider::with_provider`.

## Phase 2B — Workaround (if Path B from Task 2.2)

### Task 4: Auto-detect prefers CPU for long passes

- [ ] **4.1: Defer embedder construction until commit count is
      known.** In `crates/ohara-cli/src/commands/index.rs`, the
      current ordering builds the embedder before scanning the
      revision range. Reorder so:
      1. Resolve `--embed-provider auto` to a *candidate* provider
         only.
      2. Scan the revision range, count commits.
      3. If candidate is CoreML and commit count >
         `LONG_PASS_THRESHOLD` (initial value: 1000 — pin to the
         empirical breakpoint from Task 1.2 if available), log a
         warning and downgrade to `EmbedProvider::Cpu`.
      4. Construct the embedder.
      This avoids loading CoreML only to throw it away.

- [ ] **4.2: Explicit `--embed-provider coreml` honors the user's
      choice.** Auto-downgrade only fires when the flag is `auto`.
      Explicit `coreml` proceeds, with a one-time warning at
      startup:
      `tracing::warn!("--embed-provider coreml on long index passes is known to OOM on Apple Silicon (issue #TBD); use --embed-provider auto to fall back to CPU automatically");`
      Replace `#TBD` with the GitHub issue number filed alongside
      this work.

- [ ] **4.3: Document the workaround.** Update
      `docs-book/src/install.md` known-issue note: change "Use
      `--embed-provider cpu` for cold first-time indexes" to
      "`auto` resolves to CPU for long passes; pass `coreml`
      explicitly if you want it anyway, and expect OOM on
      multi-thousand-commit indexes."

## Phase 3 — Release

### Task 5: Ship v0.6.1

- [ ] **5.1: Bump `Cargo.toml` to `0.6.1`.**
- [ ] **5.2: Update changelog.** Under v0.6.1: state which path
      was taken (A or B) and link to
      `docs/perf/v0.6.1-leak-diagnosis.md`. For Path B, also
      describe the user-visible behaviour change of `--embed-provider auto`.
- [ ] **5.3: Move the v0.6.0 known-issue annotation** in the
      changelog from "known issue" to "fixed in v0.6.1" (Path A)
      or "documented workaround in v0.6.1" (Path B).
- [ ] **5.4: Verify cargo-dist tag format.** Read
      `dist-workspace.toml` (and the cargo-dist GitHub Actions
      workflow) before tagging — confirm the workflow triggers on
      `v*` or the specific format we use. Then
      `git tag -a v0.6.1 -m "v0.6.1 — CoreML memory-leak <fix|workaround>"`
      and watch the release workflow.

## Done when

Per RFC success criteria:

- **(A)**: cold QuestDB index with `--embed-provider coreml` finishes
  without OOM, peak `phys_footprint` < 4 GB for the duration; **or**
- **(B)**: documented workaround in place — `--embed-provider auto`
  no longer triggers the leak on a typical Apple Silicon host, and
  explicit `--embed-provider coreml` warns about the issue but stays
  honoring the user's choice.

The paired e2e quality gate from Plan 6 still passes either way (strict
gate preferred, relaxed gate acceptable for Path A only — see 3.3).

## Risk / fallback

- **Phase 1 misses 2026-05-09 deadline.** Drop to Path B
  immediately; ship v0.6.1 with the workaround and re-open the
  in-tree fix as v0.6.2 / v0.7 candidate.
- **In-tree fix introduces flakiness on CPU.** Roll back, take Path B.
- **Quality gate regresses past the relaxed top-3 bar.** Same —
  rollback, drop to Path B.
- **Repro harness can't reproduce in <10 min.** Escalate to the
  synthetic-fixture harness; if that also can't reproduce, the leak
  is workload-shaped in a way the harness misses — pause Path A and
  ship Path B.

## Files affected

Phase 1 (diagnostics, both explicitly approved by RFC):
- `tests/perf/coreml_leak_repro.rs` (new) — minimal reproduction
- `docs/perf/v0.6.1-leak-diagnosis.md` (new) — narrative of probes
  + decision

Phase 2A (in-tree fix), if taken:
- `crates/ohara-embed/src/fastembed.rs` — `EmbedderState`, recreate
  cadence, reranker mirror
- `crates/ohara-embed/Cargo.toml` — possibly a feature flag
- `tests/perf/embed_provider_paired.rs` — extend to assert
  `phys_footprint` bound; relax to top-3 if 3.3 demands it

Phase 2B (workaround), if taken:
- `crates/ohara-cli/src/commands/index.rs` — defer embedder
  construction past the commit scan; auto-downgrade heuristic
- `crates/ohara-cli/src/commands/provider.rs` — return a candidate
  for `auto` rather than a final provider
- `docs-book/src/install.md` — known-issue note becomes a workaround
  note

Phase 3:
- `Cargo.toml` — version bump
- `docs-book/src/changelog.md` — v0.6.1 entry
