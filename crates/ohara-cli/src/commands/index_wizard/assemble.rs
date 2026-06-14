//! Pure, TTY-free wizard logic: the answer types, the answer→`Args`
//! assembly, host/build provider availability, and command rendering.

use crate::commands::index::{Args, EmbedCacheArg};
use crate::commands::provider::ProviderArg;
use crate::resources::ResourcesArg;

/// Which embedding provider the user picked. `Auto` maps to *no*
/// `--embed-provider` flag (defer to the resource plan); the others
/// map to their explicit `ProviderArg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderChoice {
    #[default]
    Auto,
    Cpu,
    Coreml,
    Cuda,
}

impl ProviderChoice {
    /// `Auto` → `None` so the equivalent command omits the flag and the
    /// resource plan resolves the provider. Explicit picks pass through.
    pub fn to_provider_arg(self) -> Option<ProviderArg> {
        match self {
            ProviderChoice::Auto => None,
            ProviderChoice::Cpu => Some(ProviderArg::Cpu),
            ProviderChoice::Coreml => Some(ProviderArg::Coreml),
            ProviderChoice::Cuda => Some(ProviderArg::Cuda),
        }
    }
}

/// Which index mode the user picked. Maps onto the mutually-exclusive
/// `--incremental` / `--force` / `--rebuild --yes` flag combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeChoice {
    #[default]
    Standard,
    Incremental,
    Force,
    Rebuild,
}

/// Everything the wizard collects. Defaults match a plain
/// `ohara index` run so partially-filled answers stay sensible.
#[derive(Debug, Clone, Default)]
pub struct WizardAnswers {
    pub provider: ProviderChoice,
    pub intensity: ResourcesArg,
    pub mode: ModeChoice,
    pub threads: Option<usize>,
    pub workers: Option<usize>,
    pub commit_batch: Option<usize>,
    pub embed_batch: Option<usize>,
    pub embed_cache: EmbedCacheArg,
    pub no_progress: bool,
    pub profile: bool,
}

/// Map collected answers onto a concrete `Args`, carrying `base.path`
/// through and clearing `interactive`. This is the single place mode
/// choices become flag combinations.
pub fn assemble_args(ans: WizardAnswers, base: Args) -> Args {
    Args {
        path: base.path,
        interactive: false,
        incremental: matches!(ans.mode, ModeChoice::Incremental),
        force: matches!(ans.mode, ModeChoice::Force),
        rebuild: matches!(ans.mode, ModeChoice::Rebuild),
        yes: matches!(ans.mode, ModeChoice::Rebuild),
        commit_batch: ans.commit_batch,
        embed_batch: ans.embed_batch,
        threads: ans.threads,
        no_progress: ans.no_progress,
        profile: ans.profile,
        embed_provider: ans.provider.to_provider_arg(),
        resources: ans.intensity,
        embed_cache: ans.embed_cache,
        workers: ans.workers,
    }
}

#[cfg(test)]
mod assemble_tests {
    use super::*;
    use clap::Parser;

    fn base_args() -> Args {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: Args,
        }
        Wrapper::parse_from(["ohara"]).args
    }

    #[test]
    fn standard_mode_sets_no_mode_flags() {
        let ans = WizardAnswers::default();
        let a = assemble_args(ans, base_args());
        assert!(!a.incremental && !a.force && !a.rebuild && !a.yes);
        assert!(!a.interactive);
    }

    #[test]
    fn incremental_mode_sets_incremental_only() {
        let ans = WizardAnswers { mode: ModeChoice::Incremental, ..Default::default() };
        let a = assemble_args(ans, base_args());
        assert!(a.incremental && !a.force && !a.rebuild);
    }

    #[test]
    fn force_mode_sets_force_only() {
        let ans = WizardAnswers { mode: ModeChoice::Force, ..Default::default() };
        let a = assemble_args(ans, base_args());
        assert!(a.force && !a.incremental && !a.rebuild);
    }

    #[test]
    fn rebuild_mode_sets_rebuild_and_yes() {
        let ans = WizardAnswers { mode: ModeChoice::Rebuild, ..Default::default() };
        let a = assemble_args(ans, base_args());
        assert!(a.rebuild && a.yes);
        assert!(!a.incremental && !a.force);
    }

    #[test]
    fn provider_auto_maps_to_none() {
        let ans = WizardAnswers { provider: ProviderChoice::Auto, ..Default::default() };
        let a = assemble_args(ans, base_args());
        assert_eq!(a.embed_provider, None);
    }

    #[test]
    fn provider_explicit_maps_to_some() {
        let ans = WizardAnswers { provider: ProviderChoice::Coreml, ..Default::default() };
        let a = assemble_args(ans, base_args());
        assert_eq!(a.embed_provider, Some(ProviderArg::Coreml));
    }

    #[test]
    fn advanced_fields_pass_through() {
        let ans = WizardAnswers {
            threads: Some(4),
            workers: None,
            commit_batch: Some(512),
            embed_batch: Some(256),
            embed_cache: EmbedCacheArg::Diff,
            no_progress: true,
            profile: true,
            intensity: ResourcesArg::Aggressive,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(a.threads, Some(4));
        assert_eq!(a.workers, None);
        assert_eq!(a.commit_batch, Some(512));
        assert_eq!(a.embed_batch, Some(256));
        assert_eq!(a.embed_cache, EmbedCacheArg::Diff);
        assert!(a.no_progress && a.profile);
        assert_eq!(a.resources, ResourcesArg::Aggressive);
    }
}
