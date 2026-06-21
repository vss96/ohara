use anyhow::{bail, Result};
use clap::Args as ClapArgs;
use ohara_core::index_metadata::CompatibilityStatus;
use ohara_core::{Indexer, IndexerReport, PhaseTimings, Storage};
use std::path::PathBuf;
use std::sync::Arc;

use super::provider::{resolve_provider, ProviderArg};
use crate::resources::{apply_intensity, detect_host, pick_resources, ResourcePlan, ResourcesArg};

mod rebuild;
mod summary;
use rebuild::{assert_rebuild_safe, delete_index_files};
use summary::{failed_commits_notice, index_summary_human, phase_timings_json};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum EmbedCacheArg {
    #[default]
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
    /// Launch the interactive index wizard: choose embedding provider,
    /// resource intensity, index mode, and advanced knobs through
    /// guided prompts, preview the equivalent command, then run.
    /// Requires a TTY. Other tuning flags are ignored when `-i` is set
    /// — the wizard owns the tuning surface.
    #[arg(short, long)]
    pub interactive: bool,
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
    /// `auto`: CUDA when `CUDA_VISIBLE_DEVICES` is set, else CoreML on a
    /// CoreML-capable macOS build, else CPU). `coreml` (Apple Silicon)
    /// indexes with the fixed-shape fp32 BGE-small on the GPU+ANE — ~3x
    /// CPU throughput; first use downloads ~130MB and each run pays a
    /// one-time ~30s CoreML compile. Existing indexes stay compatible
    /// (same vector space).
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
    /// Number of worker tasks for the parallel commit pipeline
    /// (plan-28). Defaults to the number of available CPUs.
    /// `--workers 1` reproduces the serial path.
    #[arg(long)]
    pub workers: Option<usize>,
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

/// Resolve the `--embed-provider` flag and emit the CoreML first-use
/// note. Plan 30 removed the plan-7 long-pass downgrade: the
/// fixed-shape CoreML embedder has a flat memory footprint (the old
/// "leak" was per-shape specialization churn —
/// `docs/perf/v0.11-coreml-fixed-shape.md`), so the only thing worth
/// telling the user is the one-time setup cost.
fn resolve_and_note(arg: ProviderArg) -> ohara_embed::EmbedProvider {
    let provider = resolve_provider(arg);
    if matches!(provider, ohara_embed::EmbedProvider::CoreMl) {
        tracing::info!(
            "CoreML fixed-shape embedder: first use downloads the fp32 model (~130MB) \
             and each run pays a one-time ~30s CoreML compile before embedding starts.",
        );
    }
    provider
}

/// An all-zero `IndexerReport` for paths that intentionally do no work
/// (wizard print-only / cancel, and the incremental up-to-date skip).
fn noop_report() -> IndexerReport {
    IndexerReport {
        new_commits: 0,
        new_hunks: 0,
        head_symbols: 0,
        commits_failed: 0,
        phase_timings: PhaseTimings::default(),
    }
}

pub async fn run(args: Args) -> Result<IndexerReport> {
    // Interactive front-end: when `-i` is set, the wizard owns the
    // tuning surface. It returns a fully-assembled `Args` to run, an
    // equivalent command to print (user declined to run), or a cancel.
    let args = if args.interactive {
        match super::index_wizard::run_wizard_tty(args).await? {
            super::index_wizard::WizardFlow::Run(a) => a,
            super::index_wizard::WizardFlow::PrintOnly(cmd) => {
                println!("{cmd}");
                return Ok(noop_report());
            }
            super::index_wizard::WizardFlow::Cancelled => {
                eprintln!("cancelled — nothing indexed");
                return Ok(noop_report());
            }
        }
    } else {
        args
    };

    // Wall-clock starts at the very top so the summary's `total_ms`
    // covers the whole command — embedder load (which can be 15-25s
    // on first run) is part of "how long did `ohara index` take".
    let cmd_start = std::time::Instant::now();
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

    // Plan 27: guard against embed_input_mode mismatch on incremental runs.
    // When a prior index pass recorded "semantic" and the caller now requests
    // "diff" (or vice-versa), the stored KNN vectors are incompatible with
    // the new input mode — continuing would silently corrupt retrieval.
    // We skip this check for `--rebuild` because the caller has already
    // confirmed they want to delete and rebuild from scratch. A `--force`
    // refresh only replaces HEAD symbols, not the vector store, so it does
    // NOT bypass the check.
    if !args.rebuild {
        let stored_meta = storage.get_index_metadata(&repo_id).await?;
        let requested_mode = ohara_core::EmbedMode::from(args.embed_cache);
        let requested_mode_str = requested_mode.index_metadata_value();
        let runtime_for_check = ohara_core::index_metadata::runtime_metadata_from(
            ohara_embed::DEFAULT_MODEL_ID,
            ohara_embed::DEFAULT_DIM as u32,
            ohara_embed::DEFAULT_RERANKER_ID,
            ohara_parse::CHUNKER_VERSION,
            ohara_parse::parser_versions(),
            requested_mode_str,
        );
        if let CompatibilityStatus::NeedsRebuild { reason } =
            CompatibilityStatus::assess(&runtime_for_check, &stored_meta)
        {
            bail!(
                "embed_input_mode mismatch: {reason}.\n\
                 Rebuild the index with: ohara index --rebuild --yes"
            );
        }
    }

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
            return Ok(noop_report());
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

    let chosen_provider = resolve_and_note(plan.embed_provider);
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
    let embedder_load_start = std::time::Instant::now();
    // Plan 30: explicit `coreml` routes to the fixed-shape fp32
    // provider (same 384d vector space — equivalence class in
    // CompatibilityStatus); every other arm keeps the INT8 default.
    let embedder: Arc<dyn ohara_core::EmbeddingProvider> = match chosen_provider {
        ohara_embed::EmbedProvider::CoreMl => {
            tracing::info!(
                model = ohara_embed::coreml_fixed::FP32_MODEL_ID,
                "loading embedder"
            );
            let inner: Arc<dyn ohara_core::EmbeddingProvider> = Arc::new(
                tokio::task::spawn_blocking(ohara_embed::CoreMlFixedProvider::new).await??,
            );
            // Plan 31: coalesce per-commit embed calls into full
            // CoreML batches across the parallel workers — without this
            // most commits (≤8 rows) pad the 32-row model and waste
            // ~55% of the GPU. CPU/CUDA don't pad, so they skip this.
            Arc::new(ohara_embed::BatchingEmbedder::new(
                inner,
                ohara_embed::coreml_fixed::FIXED_BATCH,
            ))
        }
        other => {
            tracing::info!(model = ohara_embed::DEFAULT_MODEL_ID, "loading embedder");
            Arc::new(
                tokio::task::spawn_blocking(move || {
                    ohara_embed::FastEmbedProvider::with_provider(other)
                })
                .await??,
            )
        }
    };
    tracing::info!(
        elapsed_ms = embedder_load_start.elapsed().as_millis() as u64,
        "embedder loaded"
    );

    let commit_source = std::sync::Arc::new(ohara_git::GitCommitSource::open(&canonical)?);
    let symbol_source = std::sync::Arc::new(ohara_parse::GitSymbolSource::open(&canonical)?);

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
    let indexer = match args.workers {
        Some(n) => indexer.with_workers(n),
        None => indexer,
    };
    let report = indexer.run(&repo_id, commit_source, symbol_source).await?;
    let total_ms = cmd_start.elapsed().as_millis() as u64;
    // Two-sink summary: human-readable cosmetic block on stdout,
    // structured event on stderr (via tracing) so log aggregators /
    // CI watchdogs / a future `--json` flag see the same numbers.
    tracing::info!(
        new_commits = report.new_commits,
        new_hunks = report.new_hunks,
        head_symbols = report.head_symbols,
        total_ms = total_ms,
        "indexed"
    );
    print!(
        "{}",
        index_summary_human(
            &report.phase_timings,
            total_ms,
            report.new_commits as u64,
            report.new_hunks as u64,
            report.head_symbols as u64,
        )
    );
    if args.profile {
        // Single-line JSON keeps it `jq`-friendly and easy to
        // copy-paste into docs/perf/v0.6-baseline.md without
        // wrestling pretty-printed whitespace.
        println!("{}", phase_timings_json(&report.phase_timings));
    }
    if let Some(notice) = failed_commits_notice(report.commits_failed as u64) {
        eprintln!("{notice}");
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
mod provider_resolution_tests {
    use super::*;

    #[test]
    fn resolve_and_note_passes_every_arm_through() {
        // Plan 30: no downgrade machinery — what the user asks for is
        // what gets constructed (the CoreML arm just logs a first-use
        // note alongside).
        assert_eq!(
            resolve_and_note(ProviderArg::Cpu),
            ohara_embed::EmbedProvider::Cpu
        );
        assert_eq!(
            resolve_and_note(ProviderArg::Coreml),
            ohara_embed::EmbedProvider::CoreMl
        );
        assert_eq!(
            resolve_and_note(ProviderArg::Cuda),
            ohara_embed::EmbedProvider::Cuda
        );
        assert_eq!(
            resolve_and_note(ProviderArg::Auto),
            resolve_provider(ProviderArg::Auto)
        );
    }
}

#[cfg(test)]
mod interactive_flag_tests {
    use super::*;
    use clap::Parser;

    // Args is `#[derive(ClapArgs)]`, not a top-level `Parser`. Wrap it
    // so we can drive clap parsing in a unit test.
    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn interactive_defaults_off() {
        let w = Wrapper::parse_from(["ohara"]);
        assert!(!w.args.interactive);
    }

    #[test]
    fn long_flag_sets_interactive() {
        let w = Wrapper::parse_from(["ohara", "--interactive"]);
        assert!(w.args.interactive);
    }

    #[test]
    fn short_flag_sets_interactive() {
        let w = Wrapper::parse_from(["ohara", "-i"]);
        assert!(w.args.interactive);
    }
}
