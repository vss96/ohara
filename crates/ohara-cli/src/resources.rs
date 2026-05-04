//! Plan 6 Task 6 — `--resources auto` resource planning.
//!
//! The CLI's `--resources` flag picks reasonable defaults for
//! `--commit-batch`, `--threads`, and `--embed-provider` by consulting
//! a small lookup table keyed on host capabilities. Explicit flags
//! (`--commit-batch`, `--threads`, `--embed-provider`) **override**
//! anything `--resources` picks; the override semantics are wired in
//! `commands::index` (see the `merge_with_resource_plan` helper there).
//!
//! The lookup ranges are deliberately conservative until the v0.6
//! Phase 2 baseline is populated (see `docs/perf/v0.6-baseline.md`):
//!
//! | logical cores | commit_batch | threads | provider |
//! |---|---|---|---|
//! | <8 | 128 | cores | auto |
//! | 8..16 | 256 | cores | auto |
//! | 16+ | 512 | cores | auto |
//!
//! `conservative` halves the picked thread count and batch size;
//! `aggressive` doubles them. Both still respect the explicit-flag
//! override.

use std::num::NonZeroUsize;

use crate::commands::provider::ProviderArg;

/// User-facing resource intensity. Maps onto multipliers applied on
/// top of [`pick_resources`]'s base plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ResourcesArg {
    /// Pick batch / threads from the host (default).
    #[default]
    Auto,
    /// Halve the auto-picked batch + thread count. Use on shared dev
    /// boxes / laptops where the index pass should yield to other
    /// work.
    Conservative,
    /// Double the auto-picked batch + thread count. Use on dedicated
    /// CI runners / beefy workstations where the goal is wall time.
    Aggressive,
}

/// Snapshot of the host capabilities that drive [`pick_resources`].
///
/// Fields are populated by [`detect_host`]; the struct is exposed so
/// tests can pin a synthetic host without touching the real environment.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    /// `std::thread::available_parallelism()`, with a 1-core floor.
    pub logical_cores: usize,
    /// Total RAM in MB. Currently always 0 — populating this would
    /// pull in `sysinfo` (~10 transitive crates). The field is on the
    /// struct so `pick_resources` has a stable signature once the
    /// baseline numbers actually want a RAM-aware tier.
    pub total_ram_mb: u64,
    /// Compile-time Apple-silicon check (the same heuristic `provider`
    /// uses for `--embed-provider auto`).
    pub has_coreml: bool,
    /// `CUDA_VISIBLE_DEVICES` is set in the environment.
    pub has_cuda: bool,
}

/// Detected resource plan for one `index` run. Each field corresponds
/// to a concrete CLI flag the index command will fall back to when
/// the user didn't pass an explicit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourcePlan {
    pub commit_batch: usize,
    pub threads: usize,
    pub embed_provider: ProviderArg,
    /// Plan 15: cap on the per-commit `embed_batch` call size.
    /// Smaller values cap peak embedder allocation; larger values
    /// reduce per-commit call overhead. Default 32.
    pub embed_batch: usize,
}

/// Detect the current host. Cheap (no syscalls beyond
/// `available_parallelism`) so callers can call it once at CLI
/// startup without caching.
pub fn detect_host() -> Host {
    let logical_cores = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    let has_coreml = cfg!(target_os = "macos") && cfg!(target_arch = "aarch64");
    let has_cuda = std::env::var_os("CUDA_VISIBLE_DEVICES").is_some();
    Host {
        logical_cores,
        total_ram_mb: 0,
        has_coreml,
        has_cuda,
    }
}

/// Pick the base [`ResourcePlan`] for a host, before any
/// conservative / aggressive multiplier is applied.
///
/// The lookup table here is intentionally simple — three tiers keyed
/// on logical core count. We expect to refine the breakpoints once
/// the QuestDB baseline (Plan 6 Task 1) is populated.
pub fn pick_resources(host: &Host) -> ResourcePlan {
    let cores = host.logical_cores.max(1);
    // Threads follow cores 1:1 for now: the embedder + storage paths
    // are largely IO/ONNX-bound, so over-subscribing past CPU count
    // historically just adds context-switch overhead. The baseline
    // pass (Plan 6 Task 1) will tell us if that's worth revisiting.
    let threads = cores;
    let commit_batch = if cores < 8 {
        128
    } else if cores < 16 {
        256
    } else {
        512
    };
    let embed_batch = if cores < 8 {
        16
    } else if cores < 16 {
        32
    } else {
        64
    };
    ResourcePlan {
        commit_batch,
        threads,
        // `Auto` defers the provider choice to `provider::resolve_provider`,
        // which already consults the same Apple-silicon / CUDA-env-var
        // signals `Host` collected. Re-resolving here would just hide
        // the decision in two places.
        embed_provider: ProviderArg::Auto,
        embed_batch,
    }
}

/// Apply the user-facing intensity multiplier to a base plan.
///
/// `Conservative` halves both `commit_batch` and `threads` (with a
/// `max(1)` floor so we never produce a 0-thread pool). `Aggressive`
/// doubles both. Provider is unaffected — intensity is about throughput
/// vs. fairness, not about which accelerator to use.
pub fn apply_intensity(base: ResourcePlan, intensity: ResourcesArg) -> ResourcePlan {
    match intensity {
        ResourcesArg::Auto => base,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_with(cores: usize) -> Host {
        Host {
            logical_cores: cores,
            total_ram_mb: 0,
            has_coreml: false,
            has_cuda: false,
        }
    }

    #[test]
    fn lookup_table_low_core_box_picks_small_batch() {
        // <8 cores → 128 commit_batch, 16 embed_batch. Anchors the
        // conservative end of the table so a future tweak that reorders
        // the conditions stays caught.
        let plan = pick_resources(&host_with(4));
        assert_eq!(plan.commit_batch, 128);
        assert_eq!(plan.embed_batch, 16);
        assert_eq!(plan.threads, 4);
    }

    #[test]
    fn lookup_table_mid_core_box_picks_medium_batch() {
        // 8..16 cores → 256 commit_batch, 32 embed_batch. Threads track
        // cores 1:1.
        let plan = pick_resources(&host_with(12));
        assert_eq!(plan.commit_batch, 256);
        assert_eq!(plan.embed_batch, 32);
        assert_eq!(plan.threads, 12);
    }

    #[test]
    fn lookup_table_high_core_box_picks_default_batch() {
        // 16+ cores → 512 commit_batch, 64 embed_batch (matches the
        // existing `--commit-batch` default).
        let plan = pick_resources(&host_with(32));
        assert_eq!(plan.commit_batch, 512);
        assert_eq!(plan.embed_batch, 64);
        assert_eq!(plan.threads, 32);
    }

    #[test]
    fn lookup_table_8_core_boundary_lands_in_mid_tier() {
        // The `<8` boundary is documented in the table; pin it so we
        // notice if a refactor flips an inequality.
        let plan = pick_resources(&host_with(8));
        assert_eq!(plan.commit_batch, 256);
        assert_eq!(plan.embed_batch, 32);
    }

    #[test]
    fn lookup_table_16_core_boundary_lands_in_high_tier() {
        let plan = pick_resources(&host_with(16));
        assert_eq!(plan.commit_batch, 512);
        assert_eq!(plan.embed_batch, 64);
    }

    #[test]
    fn lookup_table_handles_zero_core_pathological_host() {
        // `available_parallelism()` is supposed to return >=1, but the
        // helper still floors to 1 defensively. A 0-core plan would
        // produce a 0-thread tokio pool downstream.
        let plan = pick_resources(&host_with(0));
        assert!(plan.threads >= 1);
        assert_eq!(plan.commit_batch, 128);
        assert_eq!(plan.embed_batch, 16);
    }

    #[test]
    fn intensity_auto_is_identity() {
        // The default arm must not perturb the base plan — that's
        // what makes the override semantics in `commands::index`
        // testable in isolation.
        let base = pick_resources(&host_with(12));
        assert_eq!(apply_intensity(base, ResourcesArg::Auto), base);
    }

    #[test]
    fn intensity_conservative_halves_batch_and_threads() {
        let base = ResourcePlan {
            commit_batch: 256,
            threads: 8,
            embed_provider: ProviderArg::Auto,
            embed_batch: 32,
        };
        let cons = apply_intensity(base, ResourcesArg::Conservative);
        assert_eq!(cons.commit_batch, 128);
        assert_eq!(cons.threads, 4);
        assert_eq!(cons.embed_batch, 16);
        // Provider is intentionally untouched; intensity is a
        // throughput knob, not an accelerator knob.
        assert_eq!(cons.embed_provider, ProviderArg::Auto);
    }

    #[test]
    fn intensity_conservative_floors_at_1() {
        // `commit_batch / 2` for very small base values would otherwise
        // produce 0, which downstream code treats as "use the default"
        // — silently undoing the intensity request.
        let base = ResourcePlan {
            commit_batch: 1,
            threads: 1,
            embed_provider: ProviderArg::Auto,
            embed_batch: 1,
        };
        let cons = apply_intensity(base, ResourcesArg::Conservative);
        assert_eq!(cons.commit_batch, 1);
        assert_eq!(cons.threads, 1);
        assert_eq!(cons.embed_batch, 1);
    }

    #[test]
    fn intensity_aggressive_doubles_batch_and_threads() {
        let base = ResourcePlan {
            commit_batch: 256,
            threads: 8,
            embed_provider: ProviderArg::Auto,
            embed_batch: 32,
        };
        let agg = apply_intensity(base, ResourcesArg::Aggressive);
        assert_eq!(agg.commit_batch, 512);
        assert_eq!(agg.threads, 16);
        assert_eq!(agg.embed_batch, 64);
    }

    #[test]
    fn intensity_aggressive_saturates_on_overflow() {
        // Defensive: `usize::MAX * 2` would otherwise overflow and
        // wrap to 0, which then trips the floor logic in
        // `Conservative` if a user re-applies. Using `saturating_mul`
        // keeps the contract monotonic.
        let base = ResourcePlan {
            commit_batch: usize::MAX,
            threads: usize::MAX,
            embed_provider: ProviderArg::Auto,
            embed_batch: usize::MAX,
        };
        let agg = apply_intensity(base, ResourcesArg::Aggressive);
        assert_eq!(agg.commit_batch, usize::MAX);
        assert_eq!(agg.threads, usize::MAX);
        assert_eq!(agg.embed_batch, usize::MAX);
    }

    #[test]
    fn detect_host_returns_at_least_one_core() {
        // `detect_host()` runs the real syscall on the test runner;
        // anything below 1 means the floor logic regressed.
        let h = detect_host();
        assert!(h.logical_cores >= 1);
    }
}
