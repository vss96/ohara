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
///
/// `coreml` routes `ohara index` to the fixed-shape CoreML embedder
/// (`ohara_embed::CoreMlFixedProvider`, ~3× CPU on Apple Silicon). On a
/// CoreML-capable build `auto` now prefers it for indexing (see
/// `detect_provider`); `--embed-provider coreml` pins it explicitly.
/// The plan-7 long-pass downgrade machinery is gone with the leak that
/// motivated it (shape-specialization churn, fixed by the static-shape
/// model; see `docs/perf/v0.11-coreml-fixed-shape.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ProviderArg {
    /// Detect from the host: `CUDA_VISIBLE_DEVICES` set → CUDA, else
    /// CoreML on a CoreML-capable macOS build, else CPU.
    #[default]
    Auto,
    Cpu,
    Coreml,
    Cuda,
}

/// Resolve a `ProviderArg` into a concrete [`EmbedProvider`].
///
/// `Auto` consults `detect_provider`. The CPU / CoreML / CUDA arms
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
/// Order of preference: CUDA when `CUDA_VISIBLE_DEVICES` is set, then
/// CoreML when the binary can actually run it
/// (`cfg!(all(feature = "coreml", target_os = "macos"))`), then CPU.
///
/// This reverses the original plan-30 default (which kept CoreML
/// opt-in): the fixed-shape path is now the steady-state indexer on
/// Apple Silicon (~3× CPU — `docs/perf/v0.11-coreml-fixed-shape.md`),
/// so `auto` prefers it on a CoreML-capable build. The trade-off is a
/// one-time ~30s CoreML compile + ~130MB first-run model download per
/// indexing process — `resolve_and_note` surfaces that to the user.
/// Two paths are deliberately insulated and never inherit this pick:
/// `ohara query` downgrades CoreML→CPU (a single query row can't
/// amortize the compile — see `commands::query`), and the CUDA branch
/// still wins when a device is visible.
pub(crate) fn detect_provider() -> EmbedProvider {
    if cuda_env_enabled() {
        return EmbedProvider::Cuda;
    }
    if coreml_available() {
        return EmbedProvider::CoreMl;
    }
    EmbedProvider::Cpu
}

/// Whether this binary can run the CoreML execution provider at all —
/// the same gate `ohara_embed::execution_providers_for` enforces. Asking
/// for CoreML without the `coreml` feature is a hard error, so `auto`
/// must only pick it when this is true.
const fn coreml_available() -> bool {
    cfg!(all(feature = "coreml", target_os = "macos"))
}

/// `CUDA_VISIBLE_DEVICES` semantics: unset, empty, and `-1` all mean
/// "no visible devices" — only a non-empty device list counts as a
/// CUDA-enabled host.
fn cuda_env_enabled() -> bool {
    match std::env::var("CUDA_VISIBLE_DEVICES") {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "-1"
        }
        Err(_) => false,
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
    fn detect_provider_on_macos_prefers_coreml_when_built_in() {
        // `auto` now prefers CoreML on a macOS build that compiled the
        // `coreml` feature (reversing the old plan-30 opt-in); without
        // the feature it still falls back to the INT8-CPU default.
        if cfg!(target_os = "macos") && std::env::var_os("CUDA_VISIBLE_DEVICES").is_none() {
            let expected = if cfg!(feature = "coreml") {
                EmbedProvider::CoreMl
            } else {
                EmbedProvider::Cpu
            };
            assert_eq!(detect_provider(), expected);
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
    fn auto_resolves_to_coreml_only_on_coreml_capable_builds() {
        // `auto` resolves to CoreML exactly when the binary can run it
        // (built with `coreml`, on macOS) and CUDA isn't visible —
        // otherwise it must never be CoreML, so a build without the
        // feature can't wander into the "not enabled" runtime error.
        let resolved = resolve_provider(ProviderArg::Auto);
        let coreml_expected = cfg!(all(feature = "coreml", target_os = "macos"))
            && std::env::var_os("CUDA_VISIBLE_DEVICES").is_none();
        assert_eq!(resolved == EmbedProvider::CoreMl, coreml_expected);
    }
}
