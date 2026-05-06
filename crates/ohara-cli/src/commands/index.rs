use anyhow::{bail, Result};
use clap::Args as ClapArgs;
use ohara_core::query::CommitsBehind;
use ohara_core::{EmbeddingProvider, Indexer, IndexerReport, PhaseTimings, Storage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::provider::{
    resolve_with_downgrade, ProviderArg, ProviderResolution, LONG_PASS_THRESHOLD,
};
use crate::resources::{apply_intensity, detect_host, pick_resources, ResourcePlan, ResourcesArg};

#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum EmbedCacheArg {
    Off,
    Semantic,
    Diff,
}

impl From<EmbedCacheArg> for ohara_core::EmbedMode {
    fn from(a: EmbedCacheArg) -> Self {
        match a {
            EmbedCacheArg::Off => ohara_core::EmbedMode::Off,
            EmbedCacheArg::Semantic => ohara_core::EmbedMode::Semantic,
            EmbedCacheArg::Diff => ohara_core::EmbedMode::Diff,
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Skip indexing (and embedder init) when HEAD is already indexed.
    /// Used by the post-commit hook so empty re-indexes are nearly free.
    #[arg(long)]
    pub incremental: bool,
    /// Force a full re-walk of HEAD symbols even when the watermark
    /// already points at HEAD. Clears existing symbol rows first so the
    /// new AST sibling-merge chunker (Plan 3 / Track C) populates the
    /// index without duplicates. Mutually exclusive with `--incremental`
    /// (force wins if both are set).
    #[arg(long)]
    pub force: bool,
    /// Plan 13: delete the existing index for this repo and rebuild
    /// from scratch. Stronger than `--force` — `--force` only refreshes
    /// HEAD-symbol rows, while `--rebuild` drops the entire commit /
    /// hunk / vector / FTS state. Used when the binary's embedder
    /// dimension or model differs from what the index was built with
    /// (`ohara status` reports `compatibility: needs rebuild`).
    /// Refuses to run unless `--yes` is also set, to keep an
    /// accidental `--rebuild` from nuking a multi-hour index pass.
    #[arg(long, conflicts_with_all = ["incremental", "force"])]
    pub rebuild: bool,
    /// Confirm a destructive operation (currently only `--rebuild`).
    /// Without this flag, `--rebuild` errors out with a one-line
    /// description of what would be deleted.
    #[arg(long, requires = "rebuild")]
    pub yes: bool,
    /// Number of commits to batch per storage transaction. Smaller =
    /// less peak RAM and more frequent fsyncs; larger = faster but
    /// uses more memory. When unset, `--resources` picks a value based
    /// on host core count.
    #[arg(long)]
    pub commit_batch: Option<usize>,
    /// Plan 15: cap on the per-commit `embed_batch` call size.
    /// Smaller values cap peak embedder allocation at the cost of
    /// more per-commit calls. When unset, `--resources` picks a
    /// value based on host core count.
    #[arg(long)]
    pub embed_batch: Option<usize>,
    /// Cap the number of threads used by the embedder's ONNX runtime.
    /// `0` means "let ort decide" (typically = CPU count). When unset,
    /// `--resources` picks a value based on host core count.
    #[arg(long)]
    pub threads: Option<usize>,
    /// Disable the progress bar even when stderr is a TTY. The indexer
    /// still emits `tracing::info!` events every 25 commits.
    #[arg(long)]
    pub no_progress: bool,
    /// Emit the per-phase wall-time + hunk-inflation breakdown as a
    /// single JSON object on stdout after the run finishes. Used by
    /// the v0.6 throughput baseline (see
    /// `docs/perf/v0.6-baseline.md`); pipe to `jq` or paste into the
    /// markdown template. The summary line still prints to stdout
    /// before the JSON; structured tracing on stderr is unaffected.
    #[arg(long)]
    pub profile: bool,
    /// ONNX execution provider for the embedder. When unset, defers to
    /// the value picked by `--resources` (which itself defaults to
    /// `auto`: CoreML on Apple silicon, CUDA when `CUDA_VISIBLE_DEVICES`
    /// is set, else CPU). CoreML / CUDA arms currently fail with a
    /// build-time-dependency error pending Plan 6 Task 3.1 follow-up.
    #[arg(long, value_enum)]
    pub embed_provider: Option<ProviderArg>,
    /// Resource intensity. `auto` (default) picks reasonable
    /// `--commit-batch` / `--threads` / `--embed-provider` values from
    /// the host's logical core count. `conservative` halves the picked
    /// batch + thread count; `aggressive` doubles them. Explicit flags
    /// always override the picked plan.
    #[arg(long, value_enum, default_value_t = ResourcesArg::Auto)]
    pub resources: ResourcesArg,
    /// Chunk-embed cache mode (plan-27). `off` (default) matches
    /// today's behavior. `semantic` caches by sha256(semantic_text);
    /// `diff` caches by sha256(diff_text) and changes the embedder
    /// input to drop the commit message.
    #[arg(long, value_enum, default_value_t = EmbedCacheArg::Off)]
    pub embed_cache: EmbedCacheArg,
}

/// Compose explicit-flag values with a [`ResourcePlan`] under the
/// override semantics from Plan 6 Task 6.2: explicit > resources >
/// default. Pulled out of `run` so the merge is unit-testable.
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

/// Render `PhaseTimings` as the JSON object emitted by `--profile`.
/// Pulled out of `run` so the JSON shape is unit-testable without
/// driving a real index pass.
pub fn phase_timings_json(pt: &PhaseTimings) -> String {
    serde_json::to_string(pt).expect("PhaseTimings serializes via derive(Serialize)")
}

/// Plan 13 Task 3.3 Step 2: refuse `--rebuild` unless the index DB
/// path resolves under `OHARA_HOME`. Defensive belt against an edge
/// case where the path resolver is replaced or `OHARA_HOME` is later
/// altered to point somewhere unexpected.
pub fn assert_rebuild_safe(db_path: &Path, ohara_home: &Path) -> Result<()> {
    if !db_path.starts_with(ohara_home) {
        bail!(
            "refusing to rebuild: index DB path {} is not inside OHARA_HOME {}",
            db_path.display(),
            ohara_home.display(),
        );
    }
    Ok(())
}

/// Plan 13 Task 3.3 Step 1: delete the index DB and its WAL / SHM
/// sidecars. Each remove is best-effort — sidecars may legitimately
/// not exist (a clean shutdown closes the WAL); only a permission /
/// I/O error on the main DB is surfaced.
pub fn delete_index_files(db_path: &Path) -> Result<()> {
    if db_path.exists() {
        std::fs::remove_file(db_path)
            .map_err(|e| anyhow::anyhow!("failed to delete index DB {}: {e}", db_path.display()))?;
    }
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        // Sidecars are advisory; ignore "not found" but surface other I/O.
        if let Err(e) = std::fs::remove_file(&sidecar) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "failed to delete sqlite sidecar {}: {e}",
                    sidecar.display()
                ));
            }
        }
    }
    Ok(())
}

/// Count commits the upcoming `index` run would walk, then resolve the
/// `--embed-provider` flag with the Plan 7 Phase 2B long-pass downgrade
/// applied.
///
/// The two log emissions live here, not in
/// [`super::provider::resolve_with_downgrade`], because they depend on
/// the runtime context (commit count, `tracing` subscriber): the
/// downgrade `warn!` so users can see why CoreML wasn't picked, and a
/// separate `warn!` when the user passed `--embed-provider coreml`
/// explicitly on Apple Silicon (the path most likely to OOM).
///
/// Returns the concrete [`ohara_embed::EmbedProvider`] the embedder
/// should be constructed with.
async fn resolve_and_warn(
    arg: ProviderArg,
    repo_id: &ohara_core::RepoId,
    storage: &Arc<ohara_storage::SqliteStorage>,
    repo_path: &std::path::Path,
) -> Result<ohara_embed::EmbedProvider> {
    let st = storage.get_index_status(repo_id).await?;
    let commits_behind = ohara_git::GitCommitsBehind::open(repo_path)?;
    let commits_to_walk = commits_behind
        .count_since(st.last_indexed_commit.as_deref())
        .await?;

    let resolution = resolve_with_downgrade(arg, commits_to_walk, LONG_PASS_THRESHOLD);
    log_resolution_warnings(arg, commits_to_walk, &resolution);
    Ok(resolution.provider)
}

/// Pure helper for the warnings emitted by [`resolve_and_warn`].
/// Pulled out so the wording is unit-testable without a real repo or
/// storage handle.
fn log_resolution_warnings(
    arg: ProviderArg,
    commits_to_walk: u64,
    resolution: &ProviderResolution,
) {
    if let Some(commits) = resolution.downgraded_from_coreml {
        tracing::warn!(
            commits,
            threshold = LONG_PASS_THRESHOLD,
            "auto-downgrading embedder from CoreML to CPU: long index pass would OOM (see docs/perf/v0.6.1-leak-diagnosis.md). \
             Pass --embed-provider coreml explicitly to bypass.",
        );
        return;
    }
    if matches!(arg, ProviderArg::Coreml)
        && cfg!(target_os = "macos")
        && cfg!(target_arch = "aarch64")
    {
        tracing::warn!(
            commits = commits_to_walk,
            "--embed-provider coreml on Apple Silicon leaks ~4 MB/batch (see docs/perf/v0.6.1-leak-diagnosis.md). \
             For long index passes use --embed-provider auto to fall back to CPU automatically.",
        );
    }
}

pub async fn run(args: Args) -> Result<IndexerReport> {
    let (repo_id, canonical, first_commit) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    tracing::info!(repo = %canonical.display(), id = repo_id.as_str(), db = %db_path.display(), "indexing");

    // Plan 13: --rebuild path. Refuse without --yes; verify the DB
    // path is under OHARA_HOME (defense-in-depth — index_db_path
    // already builds it that way, but the assertion catches any
    // future resolver change); delete the DB + its WAL / SHM
    // sidecars; then fall through to the normal index flow, which
    // will re-run migrations and rebuild every row from scratch.
    if args.rebuild {
        if !args.yes {
            bail!(
                "refusing to --rebuild without --yes: would delete {}.\n\
                 Re-run with `--rebuild --yes` to confirm.",
                db_path.display(),
            );
        }
        let home = ohara_core::paths::ohara_home()?;
        assert_rebuild_safe(&db_path, &home)?;
        tracing::warn!(db = %db_path.display(), "rebuilding: deleting existing index DB");
        delete_index_files(&db_path)?;
    }

    // Resolve the resource plan up front so the chosen values are
    // logged once and re-used everywhere downstream.
    let base_plan = pick_resources(&detect_host());
    let intensified = apply_intensity(base_plan, args.resources);
    let plan = merge_with_resource_plan(
        intensified,
        args.commit_batch,
        args.threads,
        args.embed_provider,
        args.embed_batch,
    );
    tracing::info!(
        commit_batch = plan.commit_batch,
        threads = plan.threads,
        embed_provider = ?plan.embed_provider,
        intensity = ?args.resources,
        "resource plan",
    );

    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    storage
        .open_repo(&repo_id, &canonical.to_string_lossy(), &first_commit)
        .await?;

    // --force: clear existing HEAD symbol rows so the v0.3 AST sibling-merge
    // chunker (Track C) can repopulate without duplicates. The watermark and
    // commit/hunk history are untouched — only HEAD-snapshot symbols are
    // re-extracted. `force` wins over `incremental`.
    if args.force {
        tracing::info!("force: clearing existing HEAD symbol rows");
        storage.clear_head_symbols(&repo_id).await?;
    }

    // Fast path: when --incremental is set and storage's last_indexed_commit
    // matches HEAD, return immediately without booting the FastEmbed model
    // (which costs ~hundreds of ms even when cached). This is what makes the
    // post-commit hook nearly free on no-op re-indexes.
    if args.incremental && !args.force {
        let st = storage.get_index_status(&repo_id).await?;
        let walker = ohara_git::GitWalker::open(&canonical)?;
        let head = walker.head_commit_sha()?;
        if st.last_indexed_commit.as_deref() == Some(head.as_str()) {
            tracing::info!(sha = %head, "incremental: index up-to-date, skipping embedder init");
            println!("index up-to-date at {head}");
            return Ok(IndexerReport {
                new_commits: 0,
                new_hunks: 0,
                head_symbols: 0,
                phase_timings: PhaseTimings::default(),
            });
        }
    }

    // Apply --threads before the embedder loads so the ort runtime
    // picks up the cap. ort honors `OMP_NUM_THREADS` and
    // `RAYON_NUM_THREADS` for its parallel ops; setting both is the
    // simplest cross-version knob.
    if plan.threads > 0 {
        let n = plan.threads.to_string();
        std::env::set_var("OMP_NUM_THREADS", &n);
        std::env::set_var("RAYON_NUM_THREADS", &n);
        tracing::info!(threads = plan.threads, "capping embedder threads");
    }

    let chosen_provider =
        resolve_and_warn(plan.embed_provider, &repo_id, &storage, &canonical).await?;
    tracing::info!(provider = ?chosen_provider, "embedder");

    // Construct the progress sink BEFORE the embedder loads so the
    // pre-walk spinner covers the model-load dead window. fastembed
    // lazy-loads weights inside `with_provider`, which can take 15-25s
    // on first run — without a spinner here the only output is the
    // single "embedder provider=..." log followed by silence (issue
    // #29). The same sink is later wired into the indexer so the
    // spinner upgrades into a per-commit bar once the walk completes.
    let progress: Arc<dyn ohara_core::ProgressSink> = if args.no_progress {
        Arc::new(ohara_core::NullProgress)
    } else {
        Arc::new(crate::progress::IndicatifProgress::new())
    };

    progress.pre_walk("loading embedder model");
    tracing::info!(model = ohara_embed::DEFAULT_MODEL_ID, "loading embedder");
    let embedder_load_start = std::time::Instant::now();
    let embedder = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedProvider::with_provider(chosen_provider)
        })
        .await??,
    );
    tracing::info!(
        elapsed_ms = embedder_load_start.elapsed().as_millis() as u64,
        "embedder loaded"
    );

    let commit_source = ohara_git::GitCommitSource::open(&canonical)?;
    let symbol_source = ohara_parse::GitSymbolSource::open(&canonical)?;

    // Plan 13: build the runtime metadata snapshot up front so a
    // successful pass records "this index was built with X embedder /
    // chunker / parser versions" alongside its hunks. The snapshot
    // sources truth from the live embedder handle (model + dim) plus
    // the constants owned by ohara-embed / ohara-parse / ohara-core.
    let embed_mode_for_meta = ohara_core::EmbedMode::from(args.embed_cache);
    let runtime_metadata = ohara_core::index_metadata::runtime_metadata_from(
        embedder.model_id(),
        u32::try_from(embedder.dimension()).unwrap_or(u32::MAX),
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
        embed_mode_for_meta.index_metadata_value(),
    );

    let indexer = Indexer::new(storage.clone(), embedder.clone())
        .with_batch_commits(plan.commit_batch)
        .with_embed_batch(plan.embed_batch)
        .with_progress(progress)
        .with_runtime_metadata(runtime_metadata)
        // Plan 11: enable ExactSpan hunk-symbol attribution by wiring
        // the tree-sitter atomic extractor through. Falls back to
        // HunkHeader-only attribution for files the parser can't
        // reach (binary blobs, unsupported languages); see
        // crates/ohara-core/src/hunk_attribution.rs.
        .with_atomic_symbol_extractor(Arc::new(ohara_parse::TreeSitterAtomicExtractor))
        // Plan 26: load `.oharaignore` / `.gitattributes` from the repo
        // root so the indexer respects the ignore filter automatically.
        .with_repo_root(canonical.clone())
        // Plan 27: wire the chosen embed-cache mode into the indexer so
        // the coordinator picks the right cache key strategy.
        .with_embed_mode(args.embed_cache.into());
    let report = indexer
        .run(&repo_id, &commit_source, &symbol_source)
        .await?;
    // Two-sink summary: human-readable on stdout, structured event on
    // stderr so log aggregators / CI watchdogs / a future `--json` flag
    // see the same numbers.
    tracing::info!(
        new_commits = report.new_commits,
        new_hunks = report.new_hunks,
        head_symbols = report.head_symbols,
        "indexed"
    );
    println!(
        "indexed: {} new commits, {} hunks, {} HEAD symbols",
        report.new_commits, report.new_hunks, report.head_symbols
    );
    if args.profile {
        // Single-line JSON keeps it `jq`-friendly and easy to
        // copy-paste into docs/perf/v0.6-baseline.md without
        // wrestling pretty-printed whitespace.
        println!("{}", phase_timings_json(&report.phase_timings));
    }
    notify_daemons_of_invalidation(&canonical).await;
    Ok(report)
}

/// Best-effort: notify every alive daemon that `repo_path` was re-indexed.
///
/// Failures at any step (registry missing, daemon down, IPC error) are
/// silently discarded — the next `list_alive` call prunes stale records.
async fn notify_daemons_of_invalidation(repo_path: &std::path::Path) {
    use ohara_engine::client::{registry_path, Client};
    use ohara_engine::ipc::{Request, RequestMethod};
    use ohara_engine::registry::Registry;

    let Ok(reg_path) = registry_path() else {
        return;
    };
    let Ok(reg) = Registry::open(&reg_path) else {
        return;
    };
    let Ok(alive) = reg.list_alive() else {
        return;
    };
    for d in alive {
        let req = Request {
            id: 1,
            repo_path: Some(repo_path.to_string_lossy().to_string()),
            method: RequestMethod::InvalidateRepo,
        };
        // Best-effort. Daemon down → next list_alive prunes it.
        let _ = Client::connect(&d.socket_path).call(req).await;
    }
}

#[cfg(test)]
mod rebuild_safety_tests {
    use super::*;

    #[test]
    fn assert_rebuild_safe_passes_for_path_under_ohara_home() {
        let home = PathBuf::from("/tmp/some-ohara-home");
        let db = home.join("indexes/abc/index.sqlite");
        assert_rebuild_safe(&db, &home).expect("path under home must be safe");
    }

    #[test]
    fn assert_rebuild_safe_rejects_path_outside_ohara_home() {
        // Defense-in-depth: even if a future resolver returns a path
        // outside OHARA_HOME, --rebuild must refuse rather than
        // silently delete.
        let home = PathBuf::from("/tmp/ohara-home");
        let db = PathBuf::from("/etc/passwd");
        let err = assert_rebuild_safe(&db, &home).expect_err("must reject foreign path");
        assert!(
            err.to_string().contains("not inside OHARA_HOME"),
            "rejection message should name the constraint: {err}"
        );
    }

    #[test]
    fn delete_index_files_removes_main_db_and_present_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("ix.sqlite");
        let wal = dir.path().join("ix.sqlite-wal");
        let shm = dir.path().join("ix.sqlite-shm");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(&wal, b"wal").unwrap();
        std::fs::write(&shm, b"shm").unwrap();
        delete_index_files(&db).expect("delete");
        assert!(!db.exists(), "main db must be gone");
        assert!(!wal.exists(), "wal sidecar must be gone");
        assert!(!shm.exists(), "shm sidecar must be gone");
    }

    #[test]
    fn delete_index_files_is_no_op_when_nothing_to_delete() {
        // Sidecars are advisory — a missing -wal / -shm is not an
        // error. The main DB also being absent is fine for the case
        // where --rebuild runs before any successful index pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("never-existed.sqlite");
        delete_index_files(&db).expect("missing files must be tolerated");
    }
}

#[cfg(test)]
mod profile_json_tests {
    use super::*;

    #[test]
    fn phase_timings_json_contains_every_field() {
        // Contract: every PhaseTimings field is present in the JSON
        // emitted by --profile. The lead's manual baseline run pastes
        // this output into docs/perf/v0.6-baseline.md, so a missing
        // key here breaks the template downstream.
        let pt = PhaseTimings {
            commit_walk_ms: 1,
            diff_extract_ms: 2,
            tree_sitter_parse_ms: 3,
            embed_ms: 4,
            storage_write_ms: 5,
            fts_insert_ms: 6,
            head_symbols_ms: 7,
            total_diff_bytes: 8,
            total_added_lines: 9,
        };
        let s = phase_timings_json(&pt);
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse JSON");
        for key in [
            "commit_walk_ms",
            "diff_extract_ms",
            "tree_sitter_parse_ms",
            "embed_ms",
            "storage_write_ms",
            "fts_insert_ms",
            "head_symbols_ms",
            "total_diff_bytes",
            "total_added_lines",
        ] {
            assert!(
                v.get(key).is_some(),
                "PhaseTimings JSON must expose `{key}`"
            );
        }
        assert_eq!(v.get("commit_walk_ms").and_then(|x| x.as_u64()), Some(1));
        assert_eq!(v.get("total_added_lines").and_then(|x| x.as_u64()), Some(9));
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn plan(commit_batch: usize, threads: usize) -> ResourcePlan {
        ResourcePlan {
            commit_batch,
            threads,
            embed_provider: ProviderArg::Auto,
            embed_batch: 32,
        }
    }

    #[test]
    fn merge_passes_plan_through_when_no_explicit_flags() {
        // The whole point of `--resources auto` is that an
        // unconfigured invocation gets the picked plan unmodified.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, None, None, None);
        assert_eq!(out, p);
    }

    #[test]
    fn merge_explicit_commit_batch_overrides_plan() {
        // Override semantics from Plan 6 Task 6.2: explicit > resources.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, Some(64), None, None, None);
        assert_eq!(out.commit_batch, 64);
        assert_eq!(out.threads, 8, "threads still come from the plan");
        assert_eq!(out.embed_provider, ProviderArg::Auto);
    }

    #[test]
    fn merge_explicit_threads_overrides_plan() {
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, Some(2), None, None);
        assert_eq!(out.threads, 2);
        assert_eq!(out.commit_batch, 256);
    }

    #[test]
    fn merge_explicit_provider_overrides_plan() {
        // Specifically: a `--resources aggressive` run that picked
        // `Auto` for provider must still honor `--embed-provider cpu`
        // when the user passes it, so benchmarks can pin the slow path.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, None, Some(ProviderArg::Cpu), None);
        assert_eq!(out.embed_provider, ProviderArg::Cpu);
    }

    #[test]
    fn merge_all_three_explicit_takes_no_plan_values() {
        // Sanity: when every override is set, the plan is irrelevant.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, Some(64), Some(2), Some(ProviderArg::Cpu), Some(8));
        assert_eq!(
            out,
            ResourcePlan {
                commit_batch: 64,
                threads: 2,
                embed_provider: ProviderArg::Cpu,
                embed_batch: 8,
            }
        );
    }

    #[test]
    fn explicit_embed_batch_overrides_plan() {
        // Plan 15: explicit --embed-batch wins over the resource-plan default.
        let p = plan(256, 8); // embed_batch = 32
        let merged = merge_with_resource_plan(p, None, None, None, Some(8));
        assert_eq!(merged.embed_batch, 8);
        assert_eq!(merged.commit_batch, 256, "other fields untouched");
        assert_eq!(merged.threads, 8, "other fields untouched");
    }

    #[test]
    fn unset_embed_batch_keeps_plan_default() {
        // When no explicit flag is given, the resource-plan value passes through.
        let p = plan(256, 8); // embed_batch = 32
        let merged = merge_with_resource_plan(p, None, None, None, None);
        assert_eq!(merged.embed_batch, 32);
    }
}

#[cfg(test)]
mod warning_tests {
    use super::*;
    use ohara_embed::EmbedProvider;

    /// `log_resolution_warnings` is pure (just emits `tracing` events)
    /// so it can't directly fail a test, but the branches are
    /// straightforward enough that an "it doesn't panic on every
    /// permutation" smoke test plus the
    /// `super::super::provider::tests::*` resolution tests cover the
    /// behaviour. This module exists so the function is referenced
    /// from a test target — protecting against accidental dead-code
    /// removal — and so a future structured-log assertion has a
    /// place to land.
    #[test]
    fn warnings_handle_every_resolution_permutation() {
        let downgraded = ProviderResolution {
            provider: EmbedProvider::Cpu,
            downgraded_from_coreml: Some(LONG_PASS_THRESHOLD + 1),
        };
        let coreml_passthrough = ProviderResolution {
            provider: EmbedProvider::CoreMl,
            downgraded_from_coreml: None,
        };
        let cpu_passthrough = ProviderResolution {
            provider: EmbedProvider::Cpu,
            downgraded_from_coreml: None,
        };
        for arg in [
            ProviderArg::Auto,
            ProviderArg::Cpu,
            ProviderArg::Coreml,
            ProviderArg::Cuda,
        ] {
            log_resolution_warnings(arg, 0, &cpu_passthrough);
            log_resolution_warnings(arg, 50, &coreml_passthrough);
            log_resolution_warnings(arg, LONG_PASS_THRESHOLD + 1, &downgraded);
        }
    }
}
