//! Plan 6 Task 3.2 — CLI surface for the `--embed-provider` flag.
//!
//! Lives next to `commands::index` and `commands::query` (the only
//! two commands that construct an embedder) so the flag enum + the
//! `auto` resolution helper share a module. Anything that decides
//! "CPU vs CoreML vs CUDA" at the user-facing boundary belongs here.

use clap::ValueEnum;
use ohara_embed::EmbedProvider;

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
}
