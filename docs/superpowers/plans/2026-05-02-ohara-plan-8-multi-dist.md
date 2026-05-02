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
  finishes green and the resulting binary's `--version` runs.
- [ ] **Step 3: If it fails:** read the linker error. Most likely
  fix is exporting `DEVELOPER_DIR` or running `sudo xcode-select -s`
  before the build step. Patch `crates/ohara-embed/build.rs` if the
  `xcrun --find clang` resolution is the broken link. **Stop here
  and revisit the RFC if no link works.**
- [ ] **Step 4: Delete the workflow** once it has produced a green
  run we can refer to. The probe is one-shot; the real wiring lives
  in cargo-dist's release workflow.

## Phase 2 — Per-target features in cargo-dist

### Task 2.1 — Set per-target feature flags

**Files:**
- Modify: `dist-workspace.toml`

- [ ] **Step 1: Read the current `dist-workspace.toml`** so the diff
  is minimal. Capture the existing `[dist]` block.
- [ ] **Step 2: Add a `[dist.target."aarch64-apple-darwin"]` table**
  (or whatever cargo-dist 0.31's per-target syntax is — verify against
  `cargo dist --help` and the cargo-dist book; the syntax has churned
  between minor versions). Set `features = ["coreml"]`. Leave
  `default-features` at the workspace default.
- [ ] **Step 3: Confirm `cargo dist plan`** locally surfaces the
  CoreML feature on the macOS-arm64 build and *not* on the other
  three targets. If the plan output is wrong, fix the config before
  committing.
- [ ] **Step 4: Commit** `feat(release): build coreml feature for
  aarch64-apple-darwin`.

### Task 2.2 — Add the parallel CPU-only Apple Silicon artifact

**Files:**
- Modify: `dist-workspace.toml`

- [ ] **Step 1: Add a second build entry** for `aarch64-apple-darwin`
  with `features = []` and a custom artifact name suffix `-cpu`.
  cargo-dist supports multi-build-per-target via `[[dist.builds]]` —
  verify the exact key name against the version we're pinned to.
- [ ] **Step 2: Confirm `cargo dist plan`** produces both
  `ohara-cli-aarch64-apple-darwin.tar.xz` (CoreML) and
  `ohara-cli-aarch64-apple-darwin-cpu.tar.xz` (no features).
- [ ] **Step 3: Update the installer template** if cargo-dist needs
  a hint about which asset is "default" for the curl-pipe-sh path —
  must remain the CoreML one. This is the riskiest step; if cargo-
  dist refuses to publish two assets for the same target without a
  schema change, drop Task 2.2 and ship Phase 1 only.
- [ ] **Step 4: Commit** `feat(release): publish cpu-only aarch64
  apple-darwin opt-out artifact`.

## Phase 3 — Verify axoupdater + installer behaviour

### Task 3.1 — Manifest sanity check

**Files:**
- (none — read-only check)

- [ ] **Step 1: Trigger a release dry-run** via `cargo dist build
  --print` (or whatever the dry-run incantation is in 0.31). Capture
  the resulting `dist-manifest.json`.
- [ ] **Step 2: Diff against the v0.6.1 manifest.** The
  `aarch64-apple-darwin` entry should still point at
  `ohara-cli-aarch64-apple-darwin.tar.xz` — only the *contents* of
  that artifact change. If the asset name changed, axoupdater 0.10
  will fail the upgrade — escalate before tagging.
- [ ] **Step 3: Confirm the `-update` shim asset** (used by
  axoupdater) is still emitted for the new artifact.

### Task 3.2 — Local upgrade smoke test

**Files:**
- (none — runtime test)

- [ ] **Step 1: From a v0.6.1 install,** run
  `ohara update --check` against a staging release tag (use a
  pre-release `v0.6.2-rc.1` if cargo-dist supports it; otherwise
  point axoupdater at a forked test repo).
- [ ] **Step 2: Confirm the report says** "newer version available"
  with v0.6.2 surfaced.
- [ ] **Step 3: Run `ohara update`** and confirm the binary on disk
  has been replaced (`ohara --version` shows the new SHA, file
  mtime updated).
- [ ] **Step 4: Run `ohara index fixtures/tiny/repo --embed-provider
  coreml`** against the freshly-updated binary. Expect successful
  indexing — the proof that the CoreML EP is wired into the released
  artifact.

## Phase 4 — Documentation + release

### Task 4.1 — Update install.md and changelog

**Files:**
- Modify: `docs-book/src/install.md`
- Modify: `docs-book/src/changelog.md`

- [ ] **Step 1: install.md "Build from source" section** —
  shorten. Apple Silicon users no longer need to rebuild for CoreML;
  CUDA still requires `--features cuda` from source. Add a one-line
  note about the `-cpu` opt-out artifact.
- [ ] **Step 2: install.md "Known issues"** — drop the v0.6.1
  workaround note about source-rebuild-for-CoreML; replace with a
  brief mention that v0.6.2's released binary already has CoreML
  for Apple Silicon.
- [ ] **Step 3: changelog.md** — v0.6.2 entry: "Released binary on
  `aarch64-apple-darwin` now bundles the CoreML execution provider.
  `ohara update` pulls it transparently. CPU-only opt-out artifact
  available for users who want the smaller / link-stable build."
- [ ] **Step 4: Commit** `docs: v0.6.2 install + changelog updates`.

### Task 4.2 — Cut the release

**Files:**
- Modify: `Cargo.toml` (workspace version bump)

- [ ] **Step 1: Bump workspace version** to `0.6.2`.
- [ ] **Step 2: `cargo dist plan`** one more time on the bumped
  branch. Sanity-check artifact names.
- [ ] **Step 3: Tag `v0.6.2`** and push.
  `git tag -a v0.6.2 -m "Release v0.6.2: per-host distribution
  variants" && git push origin v0.6.2`.
- [ ] **Step 4: Watch the release workflow.** ~12 min on past
  cadence. Confirm both `ohara-cli-aarch64-apple-darwin.tar.xz`
  (CoreML) and `ohara-cli-aarch64-apple-darwin-cpu.tar.xz` (CPU)
  are attached to the GitHub release.
- [ ] **Step 5: Manual upgrade smoke test** from a previously-
  installed v0.6.1 binary on the local M-series host: `ohara update`,
  then `ohara index fixtures/tiny/repo` and confirm the embedder
  log line shows `provider=CoreMl`.

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
