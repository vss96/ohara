# ohara v0.6.1 — CoreML memory leak fix plan

> **For agentic workers:** investigation-driven plan. Tasks 1-3 are
> diagnosis (no commits expected unless they produce probe scripts
> worth keeping). Task 4 onward depends on what the diagnosis says.
> The plan author updates this file in place after each phase.

**RFC:** `docs/superpowers/specs/2026-05-02-ohara-v0.6.1-coreml-leak-rfc.md`.
The success criteria there are the contract. (A) is the preferred
landing; (B) is the documented-workaround fallback if the leak is
genuinely upstream and not fixable in this release window.

## Phase 1 — Diagnose

### Task 1: Minimal reproduction harness

- [ ] **1.1: Carve out the smallest workload that reproduces the
      leak.** Goal: a script that runs in 5–10 minutes, hits the
      same 30+ GB compressed-memory shape, and doesn't require
      QuestDB. Candidates:
      - A tight Rust loop that calls
        `FastEmbedProvider::embed_batch(&[ten short strings])` in
        a loop of N iterations against `EmbedProvider::CoreMl`.
        Watch RSS via `ps -o rss` once per iteration; assert it
        plateaus.
      - If that doesn't reproduce, escalate to a full `ohara
        index` pass against a synthetic 1k-commit fixture
        (`fixtures/build_synthetic.sh`?), and bisect from there.
      - Land the harness as `tests/perf/coreml_leak_repro.rs`,
        `#[ignore]`'d like the rest of `tests/perf/`. Even if the
        leak is later fixed upstream, the test stays as a
        regression guard.

- [ ] **1.2: Capture the allocation profile.** Two complementary
      tools on macOS:
      - `instruments -t Allocations -D out.trace
        ./target/release/<harness>` — categorizes by allocator
        site. Expect Apple's `MLModel`/`MLProgram` symbols to
        dominate if the leak is in CoreML.
      - `leaks --atExit -- ./target/release/<harness>` — lighter
        weight; shows objects that survived process exit. Often
        clearer for genuine leaks than for "growth that's freed
        late".
      - Save the trace + a one-paragraph narrative to
        `docs/perf/v0.6.1-leak-diagnosis.md`. Reference which of
        the RFC's hypotheses the data supports.

- [ ] **1.3: Pin the leak source.** From the allocation report,
      identify *the* call site responsible for unbounded growth.
      The likely answers (from RFC §Hypotheses) tell us where the
      fix lives:
      - ort's CoreML graph cache → fix is upstream or in
        `apply_provider_to_init` (cap the cache, reset between
        batches).
      - fastembed's `TextEmbedding::embed` IO binding retention →
        upstream fix or our wrapper drops the embedder
        periodically.
      - CoreML.framework leak in `MLModel.compileModel` → upstream
        Apple bug; ship workaround (B).
      - Our own code → unlikely but identifiable; fix obvious.

### Task 2: Validate hypothesis

- [ ] **2.1: Probe the suspected fix in the harness.** Without
      committing any production change yet:
      - If hypothesis (1) — try
        `ort::Session::clear_kernel_caches()` between batches if
        the API exists, or rebuild the session every N batches.
      - If hypothesis (2) — try recreating `TextEmbedding` every N
        batches; see if RSS plateaus.
      - If hypothesis (3) — try `MLModel.compileModel(_, options:)`
        with a smaller cache budget if exposed.
      - If hypothesis (4) — bisect the session lifetime.
      - Document the probe outcomes in
        `docs/perf/v0.6.1-leak-diagnosis.md`. Include "tried X,
        RSS continued growing" entries — negative results matter.

- [ ] **2.2: Decide on the fix shape.** Post a one-paragraph
      decision in the diagnosis doc:
      - **Path A:** in-tree fix is viable. Outline the change
        surface (likely in
        `crates/ohara-embed/src/fastembed.rs` — recreate
        embedder after K calls, attach a probe trait so callers
        can opt into the recreate).
      - **Path B:** upstream fix is the right place; ship the
        workaround in v0.6.1 and file an issue against
        fastembed / ort / Apple.

## Phase 2A — In-tree fix (if Path A from Task 2.2)

### Task 3: Implement the fix

- [ ] **3.1: Failing test first.** Extend
      `tests/perf/coreml_leak_repro.rs` so it fails on today's
      code: assert RSS does not exceed 4 GB across N iterations
      under CoreML. The test is `#[ignore]`'d (CoreML required —
      not all CI hits it).

- [ ] **3.2: Land the fix in `ohara-embed`.** Most likely shape:
      a `recreate_after_n_batches: Option<usize>` knob on
      `FastEmbedProvider` / `FastEmbedReranker` that drops + re-
      constructs the underlying `TextEmbedding` after that many
      `embed_batch` calls. Default `None` for non-CoreML; auto-
      tunes when CoreML is the provider.

- [ ] **3.3: Quality gate.** The Plan 6 paired e2e
      (`tests/perf/embed_provider_paired.rs`) must still pass —
      rank-1 retry hit identical CPU vs CoreML.

- [ ] **3.4: Update auto-detect heuristic.** Whatever recreate
      cadence the fix picks, log it at startup:
      `tracing::info!(recreate_after = N, "embedder")`.

## Phase 2B — Workaround (if Path B from Task 2.2)

### Task 4: Auto-detect prefers CPU for long passes

- [ ] **4.1: Heuristic in `commands::index::run`.** When
      `--embed-provider auto` resolves to CoreML AND we're about
      to walk more than `LONG_PASS_THRESHOLD` commits (e.g.
      1000), log a warning + downgrade to CPU. Document the
      threshold's rationale in code (likely the bench point
      where leak becomes critical based on Task 1.2's data).

- [ ] **4.2: Explicit `--embed-provider coreml` still honors the
      user's choice.** Auto downgrade is opt-out only when the
      flag is `auto`. If the user passed `coreml` explicitly,
      respect it but log a warning naming the leak issue.

- [ ] **4.3: Document the workaround.** Update
      `docs-book/src/install.md` known-issue note: change "Use
      `--embed-provider cpu` for cold first-time indexes" to
      "auto resolves to CPU for long passes; pass `coreml`
      explicitly if you want it anyway".

## Phase 3 — Release

### Task 5: Ship v0.6.1

- [ ] **5.1: Bump `Cargo.toml` to `0.6.1`.**
- [ ] **5.2: Update changelog.** Under v0.6.1: state which path
      was taken (A or B) and link to
      `docs/perf/v0.6.1-leak-diagnosis.md`.
- [ ] **5.3: Move the v0.6.0 known-issue annotation** in the
      changelog from "known issue" to "fixed in v0.6.1" (or
      "documented workaround in v0.6.1" depending on path).
- [ ] **5.4: Tag + push.** `git tag -a v0.6.1 -m "v0.6.1 — CoreML
      memory-leak <fix|workaround>"`. Watch the cargo-dist
      release workflow.

## Done when

Per RFC success criteria:

- **(A)**: cold QuestDB index with `--embed-provider coreml` finishes
  without OOM, peak RSS < 4 GB for the duration; **or**
- **(B)**: documented workaround in place — `--embed-provider auto`
  no longer triggers the leak on a typical Apple Silicon host, and
  explicit `--embed-provider coreml` warns about the issue but stays
  honoring the user's choice.

The paired e2e quality gate from Plan 6 still passes either way.

## Risk / fallback

- **Diagnosis takes longer than a week.** Drop to Path B
  immediately; ship v0.6.1 with the workaround and re-open the
  in-tree fix as v0.6.2 / v0.7 candidate.
- **In-tree fix introduces flakiness on CPU.** Roll back, take
  Path B.
- **Quality gate regresses.** Same — rollback, drop to Path B.

## Files affected

Phase 1 (diagnostics):
- `tests/perf/coreml_leak_repro.rs` (new) — minimal reproduction
- `docs/perf/v0.6.1-leak-diagnosis.md` (new) — narrative of probes
  + decision

Phase 2A (in-tree fix), if taken:
- `crates/ohara-embed/src/fastembed.rs` — recreate cadence
- `crates/ohara-embed/Cargo.toml` — possibly a feature flag
- `tests/perf/embed_provider_paired.rs` — extend to assert
  RSS bound

Phase 2B (workaround), if taken:
- `crates/ohara-cli/src/commands/index.rs` — auto-downgrade heuristic
- `crates/ohara-cli/src/commands/provider.rs` — auto-resolution
  decision logged + downgrade
- `docs-book/src/install.md` — known-issue note becomes a workaround
  note

Phase 3:
- `Cargo.toml` — version bump
- `docs-book/src/changelog.md` — v0.6.1 entry
