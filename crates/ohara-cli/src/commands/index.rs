use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::query::CommitsBehind;
use ohara_core::{Indexer, IndexerReport, PhaseTimings, Storage};
use std::path::PathBuf;
use std::sync::Arc;

use super::provider::{
    resolve_with_downgrade, ProviderArg, ProviderResolution, LONG_PASS_THRESHOLD,
};
use crate::resources::{apply_intensity, detect_host, pick_resources, ResourcePlan, ResourcesArg};

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
    /// Number of commits to batch per storage transaction. Smaller =
    /// less peak RAM and more frequent fsyncs; larger = faster but
    /// uses more memory. When unset, `--resources` picks a value based
    /// on host core count.
    #[arg(long)]
    pub commit_batch: Option<usize>,
    /// Cap the number of threads used by the embedder's ONNX runtime.
    /// `0` means "let ort decide" (typically = CPU count). When unset,
    /// `--resources` picks a value based on host core count.
    #[arg(long)]
    pub threads: Option<usize>,
    /// Disable the progress bar even when stderr is a TTY. The indexer
    /// still emits `tracing::info!` events every 100 commits.
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
}

/// Compose explicit-flag values with a [`ResourcePlan`] under the
/// override semantics from Plan 6 Task 6.2: explicit > resources >
/// default. Pulled out of `run` so the merge is unit-testable.
pub fn merge_with_resource_plan(
    plan: ResourcePlan,
    commit_batch: Option<usize>,
    threads: Option<usize>,
    embed_provider: Option<ProviderArg>,
) -> ResourcePlan {
    ResourcePlan {
        commit_batch: commit_batch.unwrap_or(plan.commit_batch),
        threads: threads.unwrap_or(plan.threads),
        embed_provider: embed_provider.unwrap_or(plan.embed_provider),
    }
}

/// Render `PhaseTimings` as the JSON object emitted by `--profile`.
/// Pulled out of `run` so the JSON shape is unit-testable without
/// driving a real index pass.
pub fn phase_timings_json(pt: &PhaseTimings) -> String {
    serde_json::to_string(pt).expect("PhaseTimings serializes via derive(Serialize)")
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

    // Resolve the resource plan up front so the chosen values are
    // logged once and re-used everywhere downstream.
    let base_plan = pick_resources(&detect_host());
    let intensified = apply_intensity(base_plan, args.resources);
    let plan = merge_with_resource_plan(
        intensified,
        args.commit_batch,
        args.threads,
        args.embed_provider,
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
    let embedder = Arc::new(
        tokio::task::spawn_blocking(move || {
            ohara_embed::FastEmbedProvider::with_provider(chosen_provider)
        })
        .await??,
    );
    let commit_source = ohara_git::GitCommitSource::open(&canonical)?;
    let symbol_source = ohara_parse::GitSymbolSource::open(&canonical)?;

    let progress: Arc<dyn ohara_core::ProgressSink> = if args.no_progress {
        Arc::new(ohara_core::NullProgress)
    } else {
        Arc::new(crate::progress::IndicatifProgress::new())
    };

    // Plan 13: build the runtime metadata snapshot up front so a
    // successful pass records "this index was built with X embedder /
    // chunker / parser versions" alongside its hunks. The snapshot
    // sources truth from the live embedder handle (model + dim) plus
    // the constants owned by ohara-embed / ohara-parse / ohara-core.
    let runtime_metadata = ohara_core::RuntimeIndexMetadata::current(
        embedder.as_ref(),
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
    );

    let indexer = Indexer::new(storage.clone(), embedder.clone())
        .with_batch_commits(plan.commit_batch)
        .with_progress(progress)
        .with_runtime_metadata(runtime_metadata);
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
    Ok(report)
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
        }
    }

    #[test]
    fn merge_passes_plan_through_when_no_explicit_flags() {
        // The whole point of `--resources auto` is that an
        // unconfigured invocation gets the picked plan unmodified.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, None, None);
        assert_eq!(out, p);
    }

    #[test]
    fn merge_explicit_commit_batch_overrides_plan() {
        // Override semantics from Plan 6 Task 6.2: explicit > resources.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, Some(64), None, None);
        assert_eq!(out.commit_batch, 64);
        assert_eq!(out.threads, 8, "threads still come from the plan");
        assert_eq!(out.embed_provider, ProviderArg::Auto);
    }

    #[test]
    fn merge_explicit_threads_overrides_plan() {
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, Some(2), None);
        assert_eq!(out.threads, 2);
        assert_eq!(out.commit_batch, 256);
    }

    #[test]
    fn merge_explicit_provider_overrides_plan() {
        // Specifically: a `--resources aggressive` run that picked
        // `Auto` for provider must still honor `--embed-provider cpu`
        // when the user passes it, so benchmarks can pin the slow path.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, None, None, Some(ProviderArg::Cpu));
        assert_eq!(out.embed_provider, ProviderArg::Cpu);
    }

    #[test]
    fn merge_all_three_explicit_takes_no_plan_values() {
        // Sanity: when every override is set, the plan is irrelevant.
        let p = plan(256, 8);
        let out = merge_with_resource_plan(p, Some(64), Some(2), Some(ProviderArg::Cpu));
        assert_eq!(
            out,
            ResourcePlan {
                commit_batch: 64,
                threads: 2,
                embed_provider: ProviderArg::Cpu,
            }
        );
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
