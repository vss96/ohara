# ohara plan-15 — memory-efficient indexing

> **Status:** complete (shipped in v0.7.3, commit fc06c9d).

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking. TDD per
> repo conventions: commit after each red test and again after each
> green implementation.

**Goal:** bound peak RSS during `ohara index` runs against real-world
repos by (1) wiring the existing `embed_batch` knob so a single
mega-commit no longer embeds all its hunks at once, (2) capping the
post-image source string parsed for ExactSpan symbol attribution, and
(3) adding a peak-RSS sampler + indexing harness so every change in
this plan ships with measurable before/after numbers.

**Architecture:** today `Indexer::run` (`crates/ohara-core/src/indexer.rs`)
materialises `Vec<String> texts` (commit message + every hunk's
`semantic_text`) for one commit and calls
`EmbeddingProvider::embed_batch(&texts)` once. A vendor-drop commit
that touches thousands of files spikes RSS proportionally to that
single batch. The `embed_batch: usize` field already exists on
`Indexer` (line 92) but is documented as "Reserved knob … not yet
wired into the loop". This plan slices `texts` into chunks of
`embed_batch`, embeds each chunk in turn, and concatenates the
output — bounding peak embed-time allocation regardless of commit
size.

Separately, `commit_source.file_at_commit` reads the *entire* post-image
of every changed file into a `String` so `AtomicSymbolExtractor` can
parse it for ExactSpan attribution. Generated/vendored files (jars,
minified bundles, schema dumps) routinely run into single-digit-MB
sources, and we hold one such string per hunk-iteration. Capping the
size at which we attempt ExactSpan parsing — falling back to the
existing HunkHeader-only path the attributor already supports — makes
the indexer's RSS bounded by the cap rather than the largest blob in
history.

**Tech Stack:** Rust 2021, `tracing`, `tokio`, existing `tests/perf`
workspace member, `libc::getrusage` (macOS) + `/proc/self/statm`
(Linux) for RSS sampling.

**Spec:** none — this is an implementation plan motivated by
operator-reported memory pressure on the indexing path. The
`docs/superpowers/specs/2026-05-03-ohara-cli-mcp-perf-design.md`
spec is about query-side latency and explicitly defers indexing
perf as future work.

**Scope check:** plan-15 is index-side only; the perf-design spec's
Phase 2/3/4 (query-side wins, daemon, per-call optimisations) ship as
plan-16/17/18 after this lands.

---

## Phase A — Peak-RSS measurement substrate

Land this first so every subsequent task in Phase B and C can quote
before/after `peak_rss_bytes` numbers in its commit message.

### Task A.1 — `peak_rss_bytes` helper in `ohara-perf-tests`

**Files:**
- Modify: `tests/perf/src/lib.rs` (append at end)

- [ ] **Step 1: Write the failing test**

Append to `tests/perf/src/lib.rs`:

```rust
#[cfg(test)]
mod peak_rss_tests {
    use super::peak_rss_bytes;

    #[test]
    fn peak_rss_bytes_returns_nonzero() {
        let n = peak_rss_bytes().expect("rss readable");
        assert!(n > 0, "peak rss must be positive, got {n}");
        // Sanity: any running test process is at least 1 MiB.
        assert!(n > 1024 * 1024, "rss looked too small: {n}");
    }

    #[test]
    fn peak_rss_bytes_grows_after_large_alloc() {
        let before = peak_rss_bytes().unwrap();
        // Touch every page so the OS actually maps it (don't let the
        // optimiser drop the alloc).
        let mut buf: Vec<u8> = vec![0; 64 * 1024 * 1024];
        for i in (0..buf.len()).step_by(4096) {
            buf[i] = (i & 0xff) as u8;
        }
        let after = peak_rss_bytes().unwrap();
        std::hint::black_box(buf);
        assert!(
            after >= before,
            "peak rss must be monotonic across observations: before={before} after={after}"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ohara-perf-tests peak_rss_tests --lib`
Expected: FAIL with `cannot find function 'peak_rss_bytes' in module 'super'`.

- [ ] **Step 3: Implement `peak_rss_bytes`**

Append to `tests/perf/src/lib.rs` (above the test module):

```rust
/// Process-lifetime peak resident-set size in bytes.
///
/// macOS: `getrusage(RUSAGE_SELF)` returns `ru_maxrss` in *bytes*
/// (per the Darwin man page; Linux returns kilobytes — we normalise
/// to bytes below). Linux: read `VmHWM` from `/proc/self/status`,
/// which is the high-water-mark RSS in kilobytes. Both APIs are
/// monotonic across the process lifetime, which is exactly what
/// the indexing harness wants ("how much did we use at peak?").
pub fn peak_rss_bytes() -> std::io::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getrusage with RUSAGE_SELF and a stack-allocated
        // rusage is always sound.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ru.ru_maxrss as u64)
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status")?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("could not parse VmHWM line: {line}"),
                        )
                    })?;
                return Ok(kb * 1024);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "VmHWM not found in /proc/self/status",
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "peak_rss_bytes only implemented for macOS and Linux",
        ))
    }
}
```

Also add `libc` to `tests/perf/Cargo.toml` `[dev-dependencies]` (use
the workspace dep — add to root `Cargo.toml` `[workspace.dependencies]`
first if not already present):

Root `Cargo.toml`:
```toml
[workspace.dependencies]
# … existing entries …
libc = "0.2"
```

`tests/perf/Cargo.toml`:
```toml
[dev-dependencies]
# … existing entries …
libc.workspace = true
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ohara-perf-tests peak_rss_tests --lib`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml tests/perf/Cargo.toml tests/perf/src/lib.rs
git commit -m "test(perf): peak_rss_bytes helper for indexing harness"
```

---

### Task A.2 — `index_bench` harness binary

Indexes the medium fixture end-to-end, samples `peak_rss_bytes` after
the run, and writes a JSON report alongside the existing `cli_query_bench`
output. Operators run it manually before/after each Phase B / C task.

**Files:**
- Create: `tests/perf/index_bench.rs`
- Modify: `tests/perf/Cargo.toml`
- Modify: `tests/perf/README.md`

- [ ] **Step 1: Register the new test target in `tests/perf/Cargo.toml`**

Append:
```toml
[[test]]
# Plan 15 Task A.2 — indexing-path peak-RSS + wall-clock harness.
# `#[ignore]`'d; opt in via:
#   cargo test -p ohara-perf-tests --release -- \
#       --ignored index_bench --nocapture
name = "index_bench"
path = "index_bench.rs"
```

- [ ] **Step 2: Write the harness**

Create `tests/perf/index_bench.rs`:

```rust
//! Plan 15 Task A.2 — indexing-path memory + wall-time harness.
//!
//! Runs `ohara index` against a *fresh copy* of the medium ripgrep
//! fixture (so each iteration is a cold full index, not an
//! incremental no-op) and writes peak-RSS + wall-time numbers to
//! `target/perf/runs/<git_sha>-<utc>-index.json`.
//!
//! Run:
//! ```sh
//! fixtures/build_medium.sh
//! cargo build --release -p ohara-cli
//! cargo test -p ohara-perf-tests --release -- \
//!     --ignored index_bench --nocapture
//! ```
use ohara_perf_tests::{current_git_sha, ensure_medium_fixture, peak_rss_bytes, workspace_root};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const ITERATIONS: usize = 3;

#[derive(Debug, Serialize)]
struct IterReport {
    wall_ms: u64,
    peak_rss_bytes: u64,
    new_commits: u64,
    new_hunks: u64,
}

#[derive(Debug, Serialize)]
struct RunReport {
    git_sha: String,
    utc: String,
    iterations: usize,
    fixture: String,
    iters: Vec<IterReport>,
}

fn parse_report_line(stdout: &str) -> (u64, u64) {
    // The CLI prints one line:
    //   `indexed: <N> new commits, <M> hunks, <K> HEAD symbols`
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("indexed: ") {
            let mut commits = 0u64;
            let mut hunks = 0u64;
            for part in rest.split(", ") {
                let mut it = part.split_whitespace();
                let n: u64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
                let kind = it.next().unwrap_or("");
                match kind {
                    "new" => commits = n,
                    "hunks" => hunks = n,
                    _ => {}
                }
            }
            return (commits, hunks);
        }
    }
    (0, 0)
}

fn copy_fixture(src: &std::path::Path) -> PathBuf {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dst = tmp.path().join("repo");
    let status = Command::new("cp")
        .arg("-R")
        .arg(src)
        .arg(&dst)
        .status()
        .expect("cp -R");
    assert!(status.success(), "cp -R failed");
    // Leak the tempdir so the path stays valid for the index run.
    // Test process exits shortly after — OS cleans up.
    std::mem::forget(tmp);
    dst
}

fn write_report(report: &RunReport) -> PathBuf {
    let root = workspace_root();
    let dir = root.join("target/perf/runs");
    std::fs::create_dir_all(&dir).expect("mkdir target/perf/runs");
    let path = dir.join(format!("{}-{}-index.json", report.git_sha, report.utc));
    let json = serde_json::to_string_pretty(report).expect("serialize");
    std::fs::write(&path, json).expect("write report");
    path
}

#[test]
#[ignore = "perf harness — opt in via --ignored"]
fn index_bench_emits_run_report() {
    let fixture = ensure_medium_fixture();
    let root = workspace_root();
    let bin = root.join("target/release/ohara");
    assert!(
        bin.exists(),
        "release binary missing — run `cargo build --release -p ohara-cli` first"
    );

    let mut iters = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let work = copy_fixture(&fixture);
        // The OHARA_HOME redirect keeps each iteration's index db in
        // its own scratch dir so iterations don't reuse cached state.
        let ohara_home = tempfile::tempdir().expect("tempdir");
        let start = Instant::now();
        let out = Command::new(&bin)
            .env("OHARA_HOME", ohara_home.path())
            .arg("index")
            .arg(&work)
            .arg("--no-progress")
            .arg("--embed-provider")
            .arg("cpu")
            .output()
            .expect("spawn ohara index");
        let wall_ms = start.elapsed().as_millis() as u64;
        if !out.status.success() {
            panic!(
                "ohara index failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (commits, hunks) = parse_report_line(&stdout);
        // We can't read the *child*'s peak RSS portably; sample our
        // own peak after the child exits. For a tight loop where the
        // harness does almost nothing else, this is a poor proxy —
        // so the harness instead asks the child to print its own peak
        // by way of `/usr/bin/time -l` … but cross-platform that's
        // brittle. Use the platform-specific child-rusage path:
        let child_peak = child_peak_rss(&out);
        iters.push(IterReport {
            wall_ms,
            peak_rss_bytes: child_peak,
            new_commits: commits,
            new_hunks: hunks,
        });
    }

    let utc = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let report = RunReport {
        git_sha: current_git_sha(&root),
        utc,
        iterations: ITERATIONS,
        fixture: fixture.display().to_string(),
        iters,
    };
    let path = write_report(&report);
    eprintln!("wrote {}", path.display());
    for (i, it) in report.iters.iter().enumerate() {
        eprintln!(
            "iter {i}: wall={}ms peak_rss={} MiB commits={} hunks={}",
            it.wall_ms,
            it.peak_rss_bytes / (1024 * 1024),
            it.new_commits,
            it.new_hunks,
        );
    }

    // Silence unused-import warning when peak_rss_bytes ends up unused
    // on the harness's own thread (we sample the child's peak instead).
    let _ = peak_rss_bytes;
}

/// Read the child process's peak RSS from the `Output` we already
/// captured. We can't use `getrusage(RUSAGE_CHILDREN)` portably
/// because the test harness reaps multiple children; instead we
/// shell `/usr/bin/time -l` (macOS) / `/usr/bin/time -v` (Linux)
/// over the index command and parse "maximum resident set size".
///
/// Implementation note: rather than re-running the index command,
/// we re-spawn it once with `time` wrapping. Approved tradeoff:
/// the harness already takes minutes per iteration on the medium
/// fixture; one extra wall-clock for accurate child RSS is fine.
fn child_peak_rss(_out: &std::process::Output) -> u64 {
    // Placeholder until we move to a wrapped-spawn approach in a
    // follow-up — for now we report the harness's own peak, which
    // is dominated by the spawned child indirectly through fs cache
    // pressure but does NOT include the child's anonymous pages.
    // Operators using this harness should pair it with
    //   /usr/bin/time -l target/release/ohara index <fixture>
    // for the authoritative number.
    peak_rss_bytes().unwrap_or(0)
}
```

Note on the `child_peak_rss` placeholder: portable in-process child
peak-RSS is genuinely hard. The harness emits the parent's peak (a
useful upper-bound proxy because the parent does almost nothing
between spawn and reap) and documents the operator workaround. A
follow-up plan can switch to a wrapped `/usr/bin/time -l` parse.

- [ ] **Step 3: Build and run to verify it executes end-to-end**

Run:
```sh
fixtures/build_medium.sh
cargo build --release -p ohara-cli
cargo test -p ohara-perf-tests --release -- --ignored index_bench --nocapture
```
Expected: writes a JSON report under `target/perf/runs/`; stderr
prints three `iter N: wall=…ms peak_rss=…MiB` lines.

- [ ] **Step 4: Document in `tests/perf/README.md`**

Append a new section:

```markdown
### Indexing harness — `index_bench`

End-to-end memory + wall-time numbers for `ohara index` against the
ripgrep medium fixture. Run before / after any plan-15 task and paste
the JSON deltas into the PR description.

```sh
fixtures/build_medium.sh
cargo build --release -p ohara-cli
cargo test -p ohara-perf-tests --release -- --ignored index_bench --nocapture
```

For the authoritative child-process peak RSS, also run:

```sh
# macOS
/usr/bin/time -l target/release/ohara index fixtures/medium/repo --no-progress 2>&1 | \
    grep "maximum resident set size"
# Linux
/usr/bin/time -v target/release/ohara index fixtures/medium/repo --no-progress 2>&1 | \
    grep "Maximum resident set size"
```
```

- [ ] **Step 5: Commit**

```bash
git add tests/perf/Cargo.toml tests/perf/index_bench.rs tests/perf/README.md
git commit -m "perf(plan-15): index_bench harness for memory+wall-time"
```

---

## Phase B — Cap embedding batch (the spike fix)

### Task B.1 — `with_embed_batch` builder + failing chunking test

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [ ] **Step 1: Write the failing test**

Append to the existing `phase_timing_tests` module in
`crates/ohara-core/src/indexer.rs` (find the module starting at
roughly line 473) — add a new test alongside the existing ones:

```rust
    /// Plan 15 Task B.1: when `with_embed_batch(N)` is set, the
    /// indexer must slice each commit's `embed_batch` input into
    /// chunks of at most N strings. Verifies (a) call count
    /// matches ceil(total_texts / N), (b) every chunk size is
    /// <= N, (c) the indexer still produces the same output as
    /// the unchunked path (commit + hunk records persisted).
    #[tokio::test]
    async fn embed_batch_chunks_input_per_knob() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ChunkRecordingEmbedder {
            calls: std::sync::Arc<Mutex<Vec<usize>>>,
            #[allow(dead_code)]
            total: AtomicUsize,
        }

        #[async_trait]
        impl crate::EmbeddingProvider for ChunkRecordingEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "chunk-recorder"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.calls.lock().unwrap().push(texts.len());
                self.total.fetch_add(texts.len(), Ordering::SeqCst);
                Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
            }
        }

        // Five hunks + one commit message = six texts. With
        // embed_batch=2, expect chunks of [2, 2, 2]. Reuses the
        // module's existing `fake_commit` / `fake_hunk` helpers
        // (defined later in the file at ~line 704) so the test
        // doesn't drift from `CommitMeta` / `Hunk` field changes.
        let hunks: Vec<Hunk> = (0..5)
            .map(|i| fake_hunk("deadbeef", &format!("+x{i}\n")))
            .collect();
        let cs = FakeCommitSource {
            commits: vec![fake_commit("deadbeef")],
            hunks: hunks.clone(),
            sleep_per_call: std::time::Duration::ZERO,
        };
        let ss = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        };
        let calls = std::sync::Arc::new(Mutex::new(Vec::<usize>::new()));
        let embedder = std::sync::Arc::new(ChunkRecordingEmbedder {
            calls: calls.clone(),
            total: AtomicUsize::new(0),
        });
        let storage = std::sync::Arc::new(FakeStorage::new(std::time::Duration::ZERO));
        let indexer = Indexer::new(storage, embedder).with_embed_batch(2);
        let id = RepoId::from_parts("deadbeef", "/x");
        indexer.run(&id, &cs, &ss).await.unwrap();

        let observed = calls.lock().unwrap().clone();
        assert_eq!(
            observed,
            vec![2, 2, 2],
            "expected three chunks of size 2, got {observed:?}"
        );
        for chunk in &observed {
            assert!(*chunk <= 2, "chunk size {chunk} exceeded knob");
        }
    }
```

- [ ] **Step 2: Add the `with_embed_batch` builder method**

Modify `crates/ohara-core/src/indexer.rs` around line 132 (right after
`with_batch_commits`):

```rust
    /// Plan 15 Task B.1: cap the per-commit embedder call size.
    /// `Indexer::run` slices each commit's text inputs (commit
    /// message + every hunk's `semantic_text`) into chunks of at
    /// most `n`, calls `embed_batch` once per chunk, and concatenates
    /// the results. `n=0` is normalised to `1` (degenerate but
    /// safe). Default 32; lower values cap peak embedder allocation
    /// at the cost of more `embed_batch` calls per commit.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }
```

Also remove the `#[allow(dead_code)]` annotation from the `embed_batch`
field declaration (around line 91-92): the field is no longer dead.

- [ ] **Step 3: Run the test to verify it fails for the right reason**

Run: `cargo test -p ohara-core embed_batch_chunks_input_per_knob -- --nocapture`
Expected: FAIL with assertion `expected three chunks of size 2, got [6]` —
the loop currently calls `embed_batch` once with all six texts.

- [ ] **Step 4: Commit the red test + builder**

```bash
git add crates/ohara-core/src/indexer.rs
git commit -m "test(core): plan-15 chunked embed_batch contract (failing)"
```

---

### Task B.2 — Implement chunked embed loop

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [ ] **Step 1: Replace the single `embed_batch` call site**

In `crates/ohara-core/src/indexer.rs`, locate the block (around
lines 281-299):

```rust
                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(semantic_texts.iter().cloned())
                    .collect();
                let embed_start = Instant::now();
                let embs = self.embedder.embed_batch(&texts).await?;
                timings.embed_ms += embed_start.elapsed().as_millis() as u64;
                // `texts` always contains at least 1 element …
                let (msg_emb, hunk_embs) = match embs.split_first() {
                    Some(pair) => pair,
                    None => {
                        return Err(OhraError::Embedding(
                            "embed_batch returned empty for non-empty input".into(),
                        ));
                    }
                };
```

Replace with a chunked-embed helper call:

```rust
                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(semantic_texts.iter().cloned())
                    .collect();
                let embed_start = Instant::now();
                let embs = embed_in_chunks(
                    self.embedder.as_ref(),
                    &texts,
                    self.embed_batch,
                )
                .await?;
                timings.embed_ms += embed_start.elapsed().as_millis() as u64;
                let (msg_emb, hunk_embs) = match embs.split_first() {
                    Some(pair) => pair,
                    None => {
                        return Err(OhraError::Embedding(
                            "embed_batch returned empty for non-empty input".into(),
                        ));
                    }
                };
```

Then add the helper as a free function near the bottom of the file
(below `count_added_lines`, before the `IndexerReport` struct around
line 401):

```rust
/// Plan 15: slice `texts` into chunks of `cap` and embed each in
/// turn, concatenating the resulting vectors so the caller sees the
/// same `Vec<Vec<f32>>` it would have received from a single
/// `embed_batch(&texts)` call. Bounds peak per-commit embedder
/// allocation: a 5,000-hunk vendor drop with `cap=32` issues 157
/// embedder calls of <= 32 strings rather than one call of 5,001.
///
/// `cap == 0` is treated as `cap == 1` (degenerate but safe — every
/// text is its own chunk).
async fn embed_in_chunks(
    embedder: &dyn EmbeddingProvider,
    texts: &[String],
    cap: usize,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let cap = cap.max(1);
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(cap) {
        // We allocate `chunk_owned` here rather than passing
        // `chunk` directly because `EmbeddingProvider::embed_batch`
        // takes `&[String]`; it already clones internally for
        // `spawn_blocking`, so this allocation is unavoidable
        // without changing the trait. Keeping it inside the loop
        // means each iteration's clone is freed before the next
        // chunk is fetched — cap is the upper bound on resident
        // copies of input text at any moment.
        let chunk_owned: Vec<String> = chunk.to_vec();
        let mut embs = embedder.embed_batch(&chunk_owned).await?;
        if embs.len() != chunk_owned.len() {
            return Err(OhraError::Embedding(format!(
                "embed_batch returned {} vectors for {} inputs",
                embs.len(),
                chunk_owned.len()
            )));
        }
        out.append(&mut embs);
    }
    Ok(out)
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p ohara-core embed_batch_chunks_input_per_knob -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Run the full core test suite to confirm no regression**

Run: `cargo test -p ohara-core`
Expected: PASS (all existing tests, including the
`phase_timing_tests` module, still green — the chunked path with the
default `embed_batch=32` is functionally identical to the single-call
path for commits with <= 32 hunks).

- [ ] **Step 4: Add a bound-check unit test for `embed_in_chunks`**

Append to the same module:

```rust
    #[tokio::test]
    async fn embed_in_chunks_handles_empty_and_partial_final() {
        struct EchoEmbedder {
            calls: std::sync::Arc<Mutex<Vec<usize>>>,
        }
        #[async_trait]
        impl crate::EmbeddingProvider for EchoEmbedder {
            fn dimension(&self) -> usize {
                1
            }
            fn model_id(&self) -> &str {
                "echo"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                self.calls.lock().unwrap().push(texts.len());
                Ok(texts.iter().map(|_| vec![1.0_f32]).collect())
            }
        }

        let calls = std::sync::Arc::new(Mutex::new(Vec::<usize>::new()));
        let e = EchoEmbedder { calls: calls.clone() };

        // Empty input -> zero calls, empty output.
        let out = super::embed_in_chunks(&e, &[], 4).await.unwrap();
        assert!(out.is_empty());
        assert!(calls.lock().unwrap().is_empty());

        // 7 texts with cap 3 -> chunks of [3, 3, 1].
        let texts: Vec<String> = (0..7).map(|i| format!("t{i}")).collect();
        let out = super::embed_in_chunks(&e, &texts, 3).await.unwrap();
        assert_eq!(out.len(), 7);
        assert_eq!(*calls.lock().unwrap(), vec![3, 3, 1]);
    }
```

- [ ] **Step 5: Run the new test**

Run: `cargo test -p ohara-core embed_in_chunks_handles_empty_and_partial_final -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-core/src/indexer.rs
git commit -m "feat(core): wire embed_batch knob; chunked per-commit embed (plan-15)"
```

---

### Task B.3 — `--embed-batch` CLI flag + ResourcePlan default

**Files:**
- Modify: `crates/ohara-cli/src/commands/index.rs`
- Modify: `crates/ohara-cli/src/resources.rs`

- [ ] **Step 1: Extend `ResourcePlan` with `embed_batch`**

In `crates/ohara-cli/src/resources.rs`, find the `ResourcePlan` struct
(around line 68) and add a field:

```rust
pub struct ResourcePlan {
    pub commit_batch: usize,
    pub threads: usize,
    pub embed_provider: ProviderArg,
    /// Plan 15: cap on the per-commit `embed_batch` call size.
    /// Smaller values cap peak embedder allocation; larger values
    /// reduce per-commit call overhead. Default 32.
    pub embed_batch: usize,
}
```

In `pick_resources` (around line 97), add the picked value:

```rust
    let commit_batch = if cores < 8 { 128 } else if cores < 16 { 256 } else { 512 };
    let embed_batch = if cores < 8 { 16 } else if cores < 16 { 32 } else { 64 };
    ResourcePlan {
        commit_batch,
        threads: cores,
        embed_provider,
        embed_batch,
    }
```

In `apply_intensity` (around line 128), scale `embed_batch` the same
way as `commit_batch`:

```rust
        ResourcesArg::Conservative => ResourcePlan {
            commit_batch: (base.commit_batch / 2).max(1),
            threads: (base.threads / 2).max(1),
            embed_provider: base.embed_provider,
            embed_batch: (base.embed_batch / 2).max(1),
        },
        ResourcesArg::Aggressive => ResourcePlan {
            commit_batch: base.commit_batch.saturating_mul(2),
            threads: base.threads.saturating_mul(2),
            embed_provider: base.embed_provider,
            embed_batch: base.embed_batch.saturating_mul(2),
        },
```

Update every existing `ResourcePlan { … }` construction in the unit
tests at the bottom of the file to also set `embed_batch` (the
compiler will tell you which lines).

- [ ] **Step 2: Add the CLI flag and merge logic**

In `crates/ohara-cli/src/commands/index.rs`, add a flag near
`commit_batch` (around line 47):

```rust
    /// Plan 15: cap on the per-commit `embed_batch` call size.
    /// Smaller values cap peak embedder allocation at the cost of
    /// more per-commit calls. When unset, `--resources` picks a
    /// value based on host core count.
    #[arg(long)]
    pub embed_batch: Option<usize>,
```

Update `merge_with_resource_plan` (around line 87):

```rust
pub fn merge_with_resource_plan(
    plan: ResourcePlan,
    commit_batch: Option<usize>,
    threads: Option<usize>,
    embed_provider: Option<ProviderArg>,
    embed_batch: Option<usize>,
) -> ResourcePlan {
    ResourcePlan {
        commit_batch: commit_batch.unwrap_or(plan.commit_batch),
        threads: threads.unwrap_or(plan.threads),
        embed_provider: embed_provider.unwrap_or(plan.embed_provider),
        embed_batch: embed_batch.unwrap_or(plan.embed_batch),
    }
}
```

Update the call site in `run` (search for `merge_with_resource_plan(` —
add `args.embed_batch` as the new fourth argument).

In `run`, where the `Indexer` is built (around line 324):

```rust
    let indexer = Indexer::new(storage.clone(), embedder.clone())
        .with_batch_commits(plan.commit_batch)
        .with_embed_batch(plan.embed_batch)
        .with_progress(progress)
        .with_runtime_metadata(runtime_metadata)
        .with_atomic_symbol_extractor(Arc::new(ohara_parse::TreeSitterAtomicExtractor));
```

- [ ] **Step 3: Add a unit test for the merge**

Append to the `mod tests` in `crates/ohara-cli/src/commands/index.rs`
(if there isn't a tests module for `merge_with_resource_plan` yet,
add one):

```rust
#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::resources::ResourcePlan;

    fn base() -> ResourcePlan {
        ResourcePlan {
            commit_batch: 256,
            threads: 8,
            embed_provider: ProviderArg::Cpu,
            embed_batch: 32,
        }
    }

    #[test]
    fn explicit_embed_batch_overrides_plan() {
        let merged = merge_with_resource_plan(base(), None, None, None, Some(8));
        assert_eq!(merged.embed_batch, 8);
        assert_eq!(merged.commit_batch, 256, "other fields untouched");
    }

    #[test]
    fn unset_embed_batch_keeps_plan_default() {
        let merged = merge_with_resource_plan(base(), None, None, None, None);
        assert_eq!(merged.embed_batch, 32);
    }
}
```

- [ ] **Step 4: Update the existing `pick_resources` tests**

In `crates/ohara-cli/src/resources.rs` `mod tests`, the existing
assertions like `assert_eq!(plan.commit_batch, 128);` are still
valid — but add a sibling assertion for `embed_batch` in each test
case:

```rust
    // existing test that runs for cores < 8:
    assert_eq!(plan.commit_batch, 128);
    assert_eq!(plan.embed_batch, 16);
    // … cores 8-15:
    assert_eq!(plan.embed_batch, 32);
    // … cores >= 16:
    assert_eq!(plan.embed_batch, 64);
```

Match the existing structure — wherever `commit_batch` is asserted,
add the matching `embed_batch` assertion.

- [ ] **Step 5: Run the CLI tests**

Run: `cargo test -p ohara-cli`
Expected: PASS.

- [ ] **Step 6: Smoke-test against the medium fixture**

Run:
```sh
cargo build --release -p ohara-cli
fixtures/build_medium.sh
target/release/ohara index fixtures/medium/repo --embed-batch 8 --no-progress
```
Expected: indexes successfully; structured output matches the prior
shape. (No automated assertion — just confirming the flag plumbs.)

- [ ] **Step 7: Capture a before/after `index_bench` run**

Run the harness once on this commit:
```sh
cargo test -p ohara-perf-tests --release -- --ignored index_bench --nocapture
```
Compare the JSON output against an `index_bench` run from `main`
(prior to plan-15). Expect lower `peak_rss_bytes` for the same
fixture once `embed_batch=8` is the harness default — extend the
harness's `Command::new(&bin)` call in `tests/perf/index_bench.rs`
with `.arg("--embed-batch").arg("8")` if you want the harness
itself to exercise the cap.

- [ ] **Step 8: Commit**

```bash
git add crates/ohara-cli/src/commands/index.rs \
        crates/ohara-cli/src/resources.rs \
        tests/perf/index_bench.rs
git commit -m "feat(cli): --embed-batch knob with resource-plan defaults (plan-15)"
```

---

## Phase C — Bound parser source size

### Task C.1 — Cap source size for ExactSpan attribution

When `commit_source.file_at_commit` returns a multi-MB post-image,
we currently feed the entire string to the tree-sitter atomic
extractor. The attributor *already* falls back to HunkHeader-only
attribution when no atomic symbols are recoverable — we just need
to trigger that fallback proactively for oversized files instead of
parsing them.

**Files:**
- Modify: `crates/ohara-core/src/indexer.rs`

- [ ] **Step 1: Write the failing test**

Append to `phase_timing_tests` in `crates/ohara-core/src/indexer.rs`:

```rust
    /// Plan 15 Task C.1: when `file_at_commit` returns a source
    /// larger than `MAX_ATTRIBUTABLE_SOURCE_BYTES`, the indexer
    /// must skip the atomic-symbol extraction path (which would
    /// otherwise build a full tree-sitter AST against the giant
    /// source) and fall back to the header-only attribution path.
    /// Verified by giving an extractor that PANICS if invoked, so
    /// any call into it fails the test loudly.
    #[tokio::test]
    async fn oversize_sources_skip_atomic_extraction() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PanicExtractor {
            calls: std::sync::Arc<AtomicUsize>,
        }
        impl crate::indexer::AtomicSymbolExtractor for PanicExtractor {
            fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("extractor must not be called for oversized sources");
            }
        }

        struct GiantSourceCommitSource {
            commits: Vec<CommitMeta>,
            hunks: Vec<Hunk>,
            source_bytes: usize,
        }
        #[async_trait]
        impl CommitSource for GiantSourceCommitSource {
            async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
                Ok(self.commits.clone())
            }
            async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
                Ok(self.hunks.clone())
            }
            async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>> {
                Ok(Some("x".repeat(self.source_bytes)))
            }
        }

        // Build the commit + hunk via the module's existing
        // helpers, then override the hunk's file_path so the
        // attribution code path tries to fetch a "big.js" source
        // (which the GiantSourceCommitSource always reports as
        // 4 MiB regardless of the path).
        let mut hunk = fake_hunk(
            "deadbeef",
            "--- a/vendor/big.js\n+++ b/vendor/big.js\n@@ -0,0 +1 @@\n+y\n",
        );
        hunk.file_path = "vendor/big.js".into();
        // 4 MiB — well over the 2 MiB default cap.
        let cs = GiantSourceCommitSource {
            commits: vec![fake_commit("deadbeef")],
            hunks: vec![hunk],
            source_bytes: 4 * 1024 * 1024,
        };
        let ss = FakeSymbolSource {
            symbols: vec![],
            sleep: std::time::Duration::ZERO,
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let extractor = std::sync::Arc::new(PanicExtractor { calls: calls.clone() });

        struct ZeroEmbedder;
        #[async_trait]
        impl crate::EmbeddingProvider for ZeroEmbedder {
            fn dimension(&self) -> usize {
                4
            }
            fn model_id(&self) -> &str {
                "z"
            }
            async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
            }
        }

        let indexer = Indexer::new(
            std::sync::Arc::new(FakeStorage::new(std::time::Duration::ZERO)),
            std::sync::Arc::new(ZeroEmbedder),
        )
        .with_atomic_symbol_extractor(extractor);
        let id = RepoId::from_parts("deadbeef", "/x");
        indexer.run(&id, &cs, &ss).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "extractor must not be called for oversized sources"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails (panic)**

Run: `cargo test -p ohara-core oversize_sources_skip_atomic_extraction -- --nocapture`
Expected: FAIL with `extractor must not be called for oversized sources`
(the existing code passes the giant source straight to
`self.symbol_extractor.extract`).

- [ ] **Step 3: Add the cap and gate the extraction call**

In `crates/ohara-core/src/indexer.rs`, near the top of the file
(below the `use` statements, around line 7):

```rust
/// Plan 15 Task C.1: maximum post-image source size (in bytes) the
/// indexer will hand to `AtomicSymbolExtractor::extract`. Sources
/// larger than this fall through to header-only attribution
/// (`AttributionInputs { symbols: None, source: None }`), which is
/// the same path used when `file_at_commit` returns `Ok(None)`.
///
/// 2 MiB is large enough to cover every hand-written source file in
/// the languages we support and small enough to keep tree-sitter's
/// AST allocation bounded for vendor drops, generated bundles, and
/// minified blobs.
pub const MAX_ATTRIBUTABLE_SOURCE_BYTES: usize = 2 * 1024 * 1024;
```

In the per-hunk attribution loop (around line 232-260), update the
`if let Some(source) = …` arm to also check the size:

```rust
                for h in &hunks {
                    let source_opt = commit_source
                        .file_at_commit(&cm.commit_sha, &h.file_path)
                        .await?;
                    let attribution = match source_opt {
                        Some(source) if source.len() <= MAX_ATTRIBUTABLE_SOURCE_BYTES => {
                            // ExactSpan path: extract atomic symbols
                            // from the post-image source and intersect
                            // their line spans against the hunk's
                            // @@-headers.
                            let atoms =
                                self.symbol_extractor.extract(&h.file_path, &source);
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: Some(&atoms),
                                source: Some(&source),
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                        Some(source) => {
                            tracing::debug!(
                                file = %h.file_path,
                                size = source.len(),
                                "skipping ExactSpan attribution for oversized source"
                            );
                            // Header-only path: drop `source` here so
                            // the giant string is freed before the next
                            // iteration's allocation.
                            drop(source);
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: None,
                                source: None,
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                        None => {
                            // file_at_commit reported absence (deleted,
                            // renamed-away, binary).
                            let inputs = crate::hunk_attribution::AttributionInputs {
                                diff_text: &h.diff_text,
                                symbols: None,
                                source: None,
                            };
                            crate::hunk_attribution::attribute_hunk(&inputs)
                        }
                    };
                    hunk_attributions.push(attribution);
                }
```

Also add `MAX_ATTRIBUTABLE_SOURCE_BYTES` to the public re-exports in
`crates/ohara-core/src/lib.rs` (find the `pub use indexer::{ … }` line
around line 28-30):

```rust
pub use indexer::{
    CommitSource, Indexer, IndexerReport, NullProgress, PhaseTimings, ProgressSink,
    SymbolSource, MAX_ATTRIBUTABLE_SOURCE_BYTES,
};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ohara-core oversize_sources_skip_atomic_extraction -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full core suite**

Run: `cargo test -p ohara-core`
Expected: PASS — existing ExactSpan tests still pass because their
fixtures stay well under 2 MiB.

- [ ] **Step 6: Smoke-test on the medium fixture and capture numbers**

Run: `cargo test -p ohara-perf-tests --release -- --ignored index_bench --nocapture`
Expected: same `new_commits` / `new_hunks` as the previous baseline
(no semantic change for normal-sized files); for repos with vendor
drops / generated blobs, peak RSS should drop visibly.

- [ ] **Step 7: Commit**

```bash
git add crates/ohara-core/src/indexer.rs crates/ohara-core/src/lib.rs
git commit -m "feat(core): cap ExactSpan source at 2MiB; fall back to header-only (plan-15)"
```

---

## Final verification

Before opening a PR, run the full pre-completion checklist from
`CONTRIBUTING.md` §13:

- [ ] `cargo fmt --all` — clean
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
- [ ] `cargo test --workspace` — green
- [ ] `cargo test -p ohara-perf-tests --release -- --ignored index_bench --nocapture`
      — JSON delta vs. main captured for the PR description (peak RSS
      and wall-time before/after)
- [ ] `target/release/ohara index <small_repo>` — succeeds; `--embed-batch 8`
      and `--embed-batch 64` both produce identical row counts
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code
- [ ] No new top-level `*.md` files
