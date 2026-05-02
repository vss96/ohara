//! Plan 6 Task 3.2 — CLI surface for the `--embed-provider` flag.
//!
//! Lives next to `commands::index` and `commands::query` (the only
//! two commands that construct an embedder) so the flag enum + the
//! `auto` resolution helper share a module. Anything that decides
//! "CPU vs CoreML vs CUDA" at the user-facing boundary belongs here.

use clap::ValueEnum;
use ohara_embed::EmbedProvider;

/// Long-pass commit threshold for auto-downgrading CoreML → CPU.
///
/// Plan 7 Phase 2B: an `index` run that will walk more than this many
/// commits with `--embed-provider auto` resolves to CPU on Apple
/// Silicon, because the CoreML embedder leaks ~4 MB/batch
/// (`docs/perf/v0.6.1-leak-diagnosis.md`) and would OOM the host
/// before completing. Short index passes (incremental, small repos,
/// `query`) keep the auto-pick of CoreML.
///
/// Set conservatively against a typical 16–24 GB Apple Silicon host;
/// users with larger memory who want CoreML for long passes can pass
/// `--embed-provider coreml` explicitly to bypass the downgrade.
pub const LONG_PASS_THRESHOLD: u64 = 1000;

/// Clap-friendly mirror of [`EmbedProvider`] with an extra `Auto`
/// variant for "pick the best available provider for this host".
///
/// The non-CPU arms exist on this enum even when the underlying
/// build can't honour them (see [`EmbedProvider`]); the CLI surface
/// is intentionally stable across builds so `--embed-provider coreml`
/// from a script keeps the same exit behavior — succeeding on a
/// future build, failing fast today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ProviderArg {
    /// Detect from the host (Apple silicon → CoreML, `CUDA_VISIBLE_DEVICES`
    /// set → CUDA, otherwise CPU).
    #[default]
    Auto,
    Cpu,
    Coreml,
    Cuda,
}

/// Resolve a `ProviderArg` into a concrete [`EmbedProvider`].
///
/// `Auto` consults [`detect_provider`]. The CPU / CoreML / CUDA arms
/// are passed through unchanged so users can force a provider that
/// differs from the auto pick (for benchmarking, or to confirm a
/// fallback path is wired up).
pub fn resolve_provider(arg: ProviderArg) -> EmbedProvider {
    match arg {
        ProviderArg::Auto => detect_provider(),
        ProviderArg::Cpu => EmbedProvider::Cpu,
        ProviderArg::Coreml => EmbedProvider::CoreMl,
        ProviderArg::Cuda => EmbedProvider::Cuda,
    }
}

/// Outcome of [`resolve_with_downgrade`] — concrete provider plus an
/// optional note describing whether the resolution was downgraded
/// from CoreML to CPU on a long index pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderResolution {
    /// Provider the caller should construct.
    pub provider: EmbedProvider,
    /// Set when an `Auto` resolution that would otherwise pick CoreML
    /// was downgraded to CPU because `commits_to_walk` exceeded
    /// [`LONG_PASS_THRESHOLD`]. Carries the count for logging.
    pub downgraded_from_coreml: Option<u64>,
}

/// Resolve a `ProviderArg` to a concrete provider, applying the
/// long-pass downgrade rule from Plan 7 Phase 2B.
///
/// Behaviour:
/// - `Auto` + `commits_to_walk > threshold` + auto-pick is CoreML →
///   downgrade to CPU and record the count in
///   [`ProviderResolution::downgraded_from_coreml`].
/// - `Auto` + short pass, or auto-pick is not CoreML → pass through
///   to [`detect_provider`].
/// - Explicit `Cpu` / `Coreml` / `Cuda` → honour the user's choice
///   regardless of pass length. The leak warning for explicit CoreML
///   is the caller's responsibility (it depends on host
///   architecture, not on this function's output).
pub fn resolve_with_downgrade(
    arg: ProviderArg,
    commits_to_walk: u64,
    threshold: u64,
) -> ProviderResolution {
    if !matches!(arg, ProviderArg::Auto) {
        return ProviderResolution {
            provider: resolve_provider(arg),
            downgraded_from_coreml: None,
        };
    }
    let auto_pick = detect_provider();
    if matches!(auto_pick, EmbedProvider::CoreMl) && commits_to_walk > threshold {
        return ProviderResolution {
            provider: EmbedProvider::Cpu,
            downgraded_from_coreml: Some(commits_to_walk),
        };
    }
    ProviderResolution {
        provider: auto_pick,
        downgraded_from_coreml: None,
    }
}

/// Heuristic auto-detect for `--embed-provider auto`.
///
/// Order matters: a developer with a CUDA box is unlikely to be on
/// macOS, but if both signals fire we pick CoreML first because the
/// macOS-on-Apple-silicon check is exact (compile-time `cfg!`) while
/// the CUDA check is just "an env var is set", which travels with
/// shell sessions and isn't a reliable hardware signal.
pub fn detect_provider() -> EmbedProvider {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        EmbedProvider::CoreMl
    } else if std::env::var_os("CUDA_VISIBLE_DEVICES").is_some() {
        EmbedProvider::Cuda
    } else {
        EmbedProvider::Cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_arg_default_is_auto() {
        // Documents the contract relied on by the clap derive in
        // `commands::index::Args` / `commands::query::Args`: when no
        // `--embed-provider` flag is passed we land on `Auto`, not
        // `Cpu`.
        assert_eq!(ProviderArg::default(), ProviderArg::Auto);
    }

    #[test]
    fn resolve_passes_explicit_arms_through_unchanged() {
        // Explicit > auto: even if the host would auto-pick CoreML,
        // `--embed-provider cpu` must still hand us back CPU so
        // benchmarks can pin the slow path on demand.
        assert_eq!(resolve_provider(ProviderArg::Cpu), EmbedProvider::Cpu);
        assert_eq!(resolve_provider(ProviderArg::Coreml), EmbedProvider::CoreMl);
        assert_eq!(resolve_provider(ProviderArg::Cuda), EmbedProvider::Cuda);
    }

    #[test]
    fn resolve_auto_returns_a_concrete_provider() {
        // Whatever the host is, `Auto` must collapse to one of the
        // three concrete arms — never panic, never linger as some
        // sentinel value. Callers downstream rely on getting back
        // a real `EmbedProvider`.
        let p = resolve_provider(ProviderArg::Auto);
        assert!(matches!(
            p,
            EmbedProvider::Cpu | EmbedProvider::CoreMl | EmbedProvider::Cuda
        ));
    }

    #[test]
    fn detect_provider_picks_coreml_on_apple_silicon() {
        // The auto-detect heuristic is platform-specific, so the
        // assertion here is conditional on the test runner's target.
        // Together with the CUDA check below this gives us coverage
        // on every common dev/CI host without flaking when run on
        // a different one.
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert_eq!(detect_provider(), EmbedProvider::CoreMl);
        }
    }

    #[test]
    fn detect_provider_falls_back_to_cpu_on_generic_linux() {
        // Linux/x86_64 with no CUDA env var must land on CPU — that's
        // the safe baseline for CI and most cloud dev boxes.
        if cfg!(target_os = "linux")
            && cfg!(target_arch = "x86_64")
            && std::env::var_os("CUDA_VISIBLE_DEVICES").is_none()
        {
            assert_eq!(detect_provider(), EmbedProvider::Cpu);
        }
    }

    #[test]
    fn explicit_provider_arg_is_never_downgraded() {
        // Plan 7 Phase 2B contract: `--embed-provider coreml` (or cpu,
        // or cuda) must be honoured even when the index pass is long.
        // The caller is responsible for the user-visible warning when
        // explicit CoreML lands on Apple Silicon.
        let huge = LONG_PASS_THRESHOLD * 10;
        for arg in [ProviderArg::Cpu, ProviderArg::Coreml, ProviderArg::Cuda] {
            let r = resolve_with_downgrade(arg, huge, LONG_PASS_THRESHOLD);
            assert_eq!(r.provider, resolve_provider(arg));
            assert!(
                r.downgraded_from_coreml.is_none(),
                "{arg:?} must not be auto-downgraded"
            );
        }
    }

    #[test]
    fn auto_below_threshold_passes_through_to_detect() {
        // Short index passes get the auto-picked provider unchanged —
        // CoreML on Apple Silicon stays CoreML, etc. This is the
        // `query` and `index --incremental` short-circuit path.
        let r = resolve_with_downgrade(ProviderArg::Auto, 100, LONG_PASS_THRESHOLD);
        assert_eq!(r.provider, detect_provider());
        assert!(r.downgraded_from_coreml.is_none());
    }

    #[test]
    fn auto_at_threshold_does_not_downgrade() {
        // Boundary: equal to threshold is short-pass, strictly greater
        // is long-pass. This matches the `>` in the implementation
        // and keeps the threshold easy to reason about (`> 1000`
        // means "1001 or more").
        let r = resolve_with_downgrade(ProviderArg::Auto, LONG_PASS_THRESHOLD, LONG_PASS_THRESHOLD);
        assert_eq!(r.provider, detect_provider());
        assert!(r.downgraded_from_coreml.is_none());
    }

    #[test]
    fn auto_long_pass_downgrades_only_when_auto_picks_coreml() {
        // The downgrade fires iff the auto-pick would have been CoreML
        // — i.e. on Apple Silicon. CPU and CUDA auto-picks are not
        // affected (no leak observed on those paths per the v0.6.1
        // diagnosis).
        let commits = LONG_PASS_THRESHOLD + 1;
        let r = resolve_with_downgrade(ProviderArg::Auto, commits, LONG_PASS_THRESHOLD);
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert_eq!(r.provider, EmbedProvider::Cpu);
            assert_eq!(r.downgraded_from_coreml, Some(commits));
        } else {
            assert_eq!(r.provider, detect_provider());
            assert!(r.downgraded_from_coreml.is_none());
        }
    }
}
