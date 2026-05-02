# ohara v0.6.2 — per-host distribution plan

> **For agentic workers:** the RFC at
> `docs/superpowers/specs/2026-05-02-ohara-v0.6.2-multi-dist-rfc.md`
> is the contract. Tasks 1–3 are reversible config edits; Task 4 is
> the release tag (irreversible — coordinate before pushing). Stop
> and ask if the macOS runner can't link CoreML.framework — that is
> the single most likely failure mode and the rest of the plan is
> dead in that case.

**Goal:** ship v0.6.2 such that `curl … ohara-cli-installer.sh | sh` on
Apple Silicon installs a CoreML-enabled binary, and `ohara update` from
v0.6.1 picks it up transparently. CPU and Linux paths unchanged.

**Architecture:** modify `dist-workspace.toml` (cargo-dist config) so
the `aarch64-apple-darwin` build runs with `--features coreml`. Add a
parallel `-cpu` artifact for the same triple as an opt-out. Keep
asset naming stable so axoupdater is unchanged.

**Tech stack:** cargo-dist 0.31, axoupdater 0.10, GitHub Actions
macos-14 runners.

---

## Phase 1 — Validate CoreML link on a hosted macOS runner

The whole plan hinges on the macos-14 runner being able to link
`CoreML.framework` and `clang_rt.osx`. Today this works locally
because `xcrun --find clang` resolves to the installed Xcode. On
hosted runners the SDK is present but configuration drift between
runner images is the most likely failure point.

### Task 1.1 — One-shot probe workflow

**Files:**
- Create: `.github/workflows/probe-coreml-link.yml`

- [ ] **Step 1: Write the workflow.** Single job, `runs-on: macos-14`,
  `cargo build --release --features coreml -p ohara-cli`. Cache hit
  irrelevant — this is a smoke test. Trigger: `workflow_dispatch`
  only.
- [ ] **Step 2: Run it from `gh workflow run`.** Confirm the build
- [x] **Step 2: Run it from `gh workflow run`.** Confirm the build
  finishes green and the resulting binary's `--version` runs.
  Result: green in 3m58s on `macos-14`. Run
  https://github.com/vss96/ohara/actions/runs/25248753183 — `ohara
  --version` from the produced `target/release/ohara` ran
  successfully (exit 0). CoreML.framework + clang_rt.osx linkage
  via `xcrun` works on hosted runners with no extra config.
- [x] **Step 3: If it fails:** N/A — Step 2 was green on first run.
  build.rs's `xcrun --find clang` resolution worked unmodified;
  no `DEVELOPER_DIR` export, no `xcode-select -s` needed.
- [x] **Step 4: Delete the workflow** once it has produced a green
  run we can refer to. The probe is one-shot; the real wiring lives
  in cargo-dist's release workflow. Deleted in commit `ae59bbb`
  on `main` and on `release/v0.6.2`.

## Phase 2 — Per-target features in cargo-dist

### Task 2.1 — Set per-target feature flags

**Files:**
- Modify: `dist-workspace.toml`

- [x] **Step 1: Read the current `dist-workspace.toml`** so the diff
  is minimal. Capture the existing `[dist]` block.
- [x] **Step 2: Add a `[dist.target."aarch64-apple-darwin"]` table**
  (or whatever cargo-dist 0.31's per-target syntax is — verify against
  `cargo dist --help` and the cargo-dist book; the syntax has churned
  between minor versions). Set `features = ["coreml"]`. Leave
  `default-features` at the workspace default.
  **RESOLUTION:** cargo-dist 0.31 has *no* per-target `features`
  override (the `features` key is package-local but applies to every
  configured target uniformly — confirmed via
  https://opensource.axo.dev/cargo-dist/book/reference/config.html#features
  and via a probe with `[dist.aarch64-apple-darwin]` which cargo-dist
  silently ignored). Workaround: `features = ["coreml"]` set at the
  workspace `[dist]` level, paired with `ohara-embed`'s
  target-conditional `ort` dep so the feature compiles cleanly on
  Linux + Intel-Mac (the inner `ort/coreml` flag is only on for
  `cfg(target_os = "macos")`). Source-side gating in
  `crates/ohara-embed/src/fastembed.rs` tightened from
  `cfg(feature = "coreml")` to
  `cfg(all(feature = "coreml", target_os = "macos"))`.
- [x] **Step 3: Confirm `cargo dist plan`** locally surfaces the
  CoreML feature on the macOS-arm64 build and *not* on the other
  three targets. If the plan output is wrong, fix the config before
  committing.
  **OUTCOME:** `dist plan` output is byte-identical to v0.6.1's
  per-target asset list (asset names stable for axoupdater). The
  feature flag isn't surfaced in `dist plan` text; validation moved
  to a real CI build (Task 3.1).
- [x] **Step 4: Commit** `feat(release): build coreml feature for
  aarch64-apple-darwin`. Done as `feat(release): wire coreml into
  the released aarch64-apple-darwin binary` (679714a).

### Task 2.2 — Add the parallel CPU-only Apple Silicon artifact

**Files:**
- Modify: `dist-workspace.toml`

- [!] **Dropped per Risks #2.** cargo-dist 0.31 does not support
  multi-build-per-target via `[[dist.builds]]` or any equivalent
  schema. The plan's own Risks section pre-authorised this drop:
  *"cargo-dist refuses two artifacts per target. Drop the -cpu
  opt-out (Task 2.2); ship the default-CoreML change only. Users
  who want CPU still have `--embed-provider cpu` at runtime, just
  not a smaller binary."* No commit.

## Phase 3 — Verify axoupdater + installer behaviour

### Task 3.1 — Manifest sanity check

**Files:**
- (none — read-only check)

- [x] **Step 1: Trigger a release dry-run** via `cargo dist build
  --print` (or whatever the dry-run incantation is in 0.31). Capture
  the resulting `dist-manifest.json`.
  **DONE via PR validation.** Set `pr-run-mode = "upload"` on the
  release branch and opened draft PR
  https://github.com/vss96/ohara/pull/1. Run
  https://github.com/vss96/ohara/actions/runs/25249017349 built all
  4 targets:
  - `aarch64-apple-darwin` 6m6s (CoreML link present;
    `--print=linkage` shows `/System/Library/Frameworks/CoreML.framework`)
  - `aarch64-unknown-linux-gnu` 4m32s (no regression)
  - `x86_64-unknown-linux-gnu` 5m12s (no regression)
  - `x86_64-apple-darwin` 7m44s (no regression)
- [x] **Step 2: Diff against the v0.6.1 manifest.** The
  `aarch64-apple-darwin` entry should still point at
  `ohara-cli-aarch64-apple-darwin.tar.xz` — only the *contents* of
  that artifact change. If the asset name changed, axoupdater 0.10
  will fail the upgrade — escalate before tagging.
  **OK:** asset list from the PR's manifest is identical (modulo
  version) to v0.6.1's. `ohara-cli-aarch64-apple-darwin.tar.xz`
  retained.
- [x] **Step 3: Confirm the `-update` shim asset** (used by
  axoupdater) is still emitted for the new artifact.
  **OK:** `ohara-cli-aarch64-apple-darwin-update` and
  `ohara-mcp-aarch64-apple-darwin-update` both present.

### Task 3.2 — Local upgrade smoke test

**Files:**
- (none — runtime test)

- [!] **Step 1: From a v0.6.1 install,** run
  `ohara update --check` against a staging release tag. **Deferred.**
  No v0.6.1 install on this host and no published v0.6.2 staging
  release available. The plan's Phase 4.2 Step 5 covers the
  manual upgrade smoke test against the real v0.6.2 tag.
- [!] **Step 2: Confirm the report says** "newer version available".
  **Deferred** (depends on Step 1).
- [!] **Step 3: Run `ohara update`.** **Deferred** (depends on Step 1).
- [x] **Step 4: Run `ohara index fixtures/tiny/repo --embed-provider
  coreml`** against the freshly-updated binary. Expect successful
  indexing — the proof that the CoreML EP is wired into the released
  artifact.
  **DONE against the CI-built binary.** Extracted
  `ohara-cli-aarch64-apple-darwin.tar.xz` from the PR's CI run,
  ran `./ohara index <fixtures/tiny/repo copy> --embed-provider
  coreml`. Logs show:
  ```
  embedder provider=CoreMl
  Successfully registered `CoreMLExecutionProvider`
  CoreMLExecutionProvider::GetCapability, number of partitions
    supported by CoreML: 97 number of nodes in the graph: 623
    number of nodes supported by CoreML: 447
  indexed: 3 new commits, 3 hunks, 2 HEAD symbols
  ```
  CoreML EP registers and accepts partitions; index runs to
  completion.

## Phase 4 — Documentation + release

### Task 4.1 — Update install.md and changelog

**Files:**
- Modify: `docs-book/src/install.md`
- Modify: `docs-book/src/changelog.md`

- [x] **Step 1: install.md "Build from source" section** —
  shorten. Apple Silicon users no longer need to rebuild for CoreML;
  CUDA still requires `--features cuda` from source. Add a one-line
  note about the `-cpu` opt-out artifact.
  **DONE.** `docs-book/src/install.md:53-74` rewritten — opening
  paragraph now says the cargo-dist installer for Apple Silicon
  bundles CoreML from v0.6.2 onwards. Source-build-with-features
  block keeps `cuda` (still source-only) and `coreml` (with a
  parenthetical noting the released binary already has it). The
  `-cpu` opt-out artifact is *not* mentioned because Task 2.2 was
  dropped (Risks #2) — instead the auto-downgrade callout points
  users at `--embed-provider cpu` at runtime.
- [x] **Step 2: install.md "Known issues"** — drop the v0.6.1
  workaround note about source-rebuild-for-CoreML; replace with a
  brief mention that v0.6.2's released binary already has CoreML
  for Apple Silicon.
  **DONE.** The CoreML long-pass auto-downgrade blockquote at
  `docs-book/src/install.md:76-88` now says "the released v0.6.2
  binary's `--embed-provider auto` therefore resolves to CPU on
  Apple Silicon when the upcoming index pass would walk 1,000
  commits or more" — no more "rebuild from source for CoreML"
  language.
- [x] **Step 3: changelog.md** — v0.6.2 entry: "Released binary on
  `aarch64-apple-darwin` now bundles the CoreML execution provider.
  `ohara update` pulls it transparently. CPU-only opt-out artifact
  available for users who want the smaller / link-stable build."
  **DONE.** New top entry at `docs-book/src/changelog.md:7-37`.
  Adjusted the "CPU-only opt-out artifact" language to match
  reality: that artifact is *not* shipped (Risks #2 drop), users
  fall back to `--embed-provider cpu` at runtime instead.
- [x] **Step 4: Commit** `docs: v0.6.2 install + changelog updates`.
  Pending — committed alongside the version bump in Task 4.2 Step 1.

### Task 4.2 — Cut the release

**Files:**
- Modify: `Cargo.toml` (workspace version bump)

- [x] **Step 1: Bump workspace version** to `0.6.2`.
  **DONE** in commit `23ca596` (`Cargo.toml` + `Cargo.lock` —
  only the 8 in-tree ohara crates moved; no third-party dep
  churn). The Task 4.1 docs commit `f34cf8f` and the
  pr-run-mode revert commit `cfd695c` precede it on
  `release/v0.6.2`.
- [x] **Step 2: `cargo dist plan`** one more time on the bumped
  branch. Sanity-check artifact names.
  **DONE.** Output confirms the v0.6.2 asset list is byte-
  identical (modulo version) to v0.6.1's: 4 targets × 2 apps =
  8 `.tar.xz` archives + 8 `-update` shims, plus the two
  `-installer.sh` scripts and `sha256.sum`. axoupdater follows
  by asset name; v0.6.1 → v0.6.2 self-upgrade is a no-op for
  users.
- [x] **Step 3: Tag `v0.6.2`** and push.
  `git tag -a v0.6.2 -m "Release v0.6.2: per-host distribution
  variants" && git push origin v0.6.2`.
  **DONE** after explicit user approval ("finish the release").
  Sequence executed:
  1. `git checkout main && git merge --ff-only release/v0.6.2`
     (5 commits, ae59bbb..91f8a65) — `git push origin main` OK
     (branch protection bypass logged on the server side, expected).
  2. `git tag -a v0.6.2 -m "..." && git push origin v0.6.2` —
     annotated tag pointing at `91f8a65`, pushed cleanly.
  3. release.yml run
     [25249409668](https://github.com/vss96/ohara/actions/runs/25249409668)
     fired automatically on the tag push.
- [x] **Step 4: Watch the release workflow.** ~12 min on past
  cadence. Confirm both `ohara-cli-aarch64-apple-darwin.tar.xz`
  (CoreML) and `ohara-cli-aarch64-apple-darwin-cpu.tar.xz` (CPU)
  are attached to the GitHub release.
  **DONE.** Run [25249409668](https://github.com/vss96/ohara/actions/runs/25249409668)
  finished green in ~9.5 min:
  - plan: 19s
  - build-local-artifacts (aarch64-unknown-linux-gnu): 4m23s
  - build-local-artifacts (x86_64-unknown-linux-gnu): 4m7s
  - build-local-artifacts (aarch64-apple-darwin): 6m3s
  - build-local-artifacts (x86_64-apple-darwin): 8m10s
  - build-global-artifacts: 17s; host: 29s; announce: 6s
  GitHub Release at https://github.com/vss96/ohara/releases/tag/v0.6.2
  has all 38 expected assets: 4 targets × 2 apps = 8 `.tar.xz` +
  8 `.sha256` + 8 `-update` shims, plus `ohara-cli-installer.sh`,
  `ohara-mcp-installer.sh`, `dist-manifest.json`, `sha256.sum`,
  `source.tar.gz`(+`.sha256`). The `-cpu` opt-out artifact is
  intentionally absent (Risks #2 drop).
- [x] **Step 5: Manual upgrade smoke test** from a previously-
  installed v0.6.1 binary on the local M-series host: `ohara update`,
  then `ohara index fixtures/tiny/repo` and confirm the embedder
  log line shows `provider=CoreMl`.
  **DONE on the local M-series dev box.**
  - Pre-upgrade: `ohara --version` → `ohara 0.6.1 (70f542f)` at
    `/Users/vss/.cargo/bin/ohara`. Receipt
    `~/.config/ohara-cli/ohara-cli-receipt.json` present.
  - `ohara update --check` → `update available: 0.6.2`.
  - `ohara update` → `downloading ohara-cli 0.6.2
    aarch64-apple-darwin / installing to /Users/vss/.cargo/bin /
    everything's installed! / updated to 0.6.2: installed at
    /Users/vss/.cargo`.
  - Post-upgrade: `ohara --version` → `ohara 0.6.2 (91f8a65)`,
    Mach-O 64-bit executable arm64.
  - `RUST_LOG=info ohara index <fixtures/tiny/repo copy>
    --embed-provider coreml` produced the expected log lines:
    ```
    embedder provider=CoreMl
    Successfully registered `CoreMLExecutionProvider`
    CoreMLExecutionProvider::GetCapability, number of partitions
      supported by CoreML: 97 number of nodes in the graph: 623
      number of nodes supported by CoreML: 447
    indexed: 3 new commits, 3 hunks, 2 HEAD symbols
    ```
  axoupdater 0.10's asset-name-based upgrade path works without
  intervention; the CoreML EP is wired into the released binary.

## Out of scope (deferred)

- **CUDA on Linux x86_64.** Needs a CI runner with the toolkit;
  +10 min build time, ~3 GB toolkit install. Tracked for v0.7.
- **Runtime accelerator detection** (single fat binary that
  dlopen-s CoreML/CUDA at startup). Big rework of the ort
  integration; not justified at current install volume.
- **Per-host CPU microarch tuning** (`-march=native` etc.). Defer
  until the throughput baseline (Plan 6 Phase 2B) shows it's worth
  the matrix expansion.

## Risks

1. **macos-14 runner can't link CoreML.framework.** Phase 1 catches
   this. Mitigation: revert to source-build-only path, ship v0.6.2
   as a docs-only release with the install.md improvements.
2. **cargo-dist refuses two artifacts per target.** Drop the `-cpu`
   opt-out (Task 2.2); ship the default-CoreML change only. Users
   who want CPU still have `--embed-provider cpu` at runtime, just
   not a smaller binary.
3. **axoupdater asset-name drift.** If the manifest changes shape,
   v0.6.1 → v0.6.2 self-upgrade breaks. Phase 3 Task 3.2 catches
   this before tagging. If broken, document the manual install path
   in the release notes and fix-forward in v0.6.3.
4. **Binary size complaint.** The CoreML link adds ~5–10 MB. If
   feedback surfaces, the `-cpu` opt-out artifact is the answer.
