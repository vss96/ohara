//! Pure, TTY-free wizard logic: the answer types, the answer→`Args`
//! assembly, host/build provider availability, and command rendering.

use crate::commands::index::{Args, EmbedCacheArg};
use crate::commands::provider::{resolve_provider, ProviderArg};
use crate::resources::ResourcesArg;
use ohara_embed::EmbedProvider;

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

/// What this binary + host can offer for `--embed-provider`. Computed
/// once by [`host_capabilities`]; `provider_choices` is a pure function
/// of it so the list logic is unit-testable without cfg gymnastics.
#[derive(Debug, Clone, Copy)]
pub struct ProviderAvailability {
    /// `cfg!(target_os = "macos")`.
    pub macos: bool,
    /// `cfg!(feature = "coreml")` — CoreML compiled into this build.
    pub coreml_build: bool,
    /// `cfg!(feature = "cuda")` — CUDA compiled into this build.
    pub cuda_build: bool,
    /// What `--embed-provider auto` resolves to here: "CPU" / "CUDA"
    /// (never "CoreML" — auto is opt-out of CoreML, plan-30).
    pub auto_label: &'static str,
}

impl ProviderAvailability {
    fn coreml_offerable(&self) -> bool {
        self.macos && self.coreml_build
    }
}

/// Detect provider availability from build features + the same
/// auto-resolution the index path uses.
pub fn host_capabilities() -> ProviderAvailability {
    let auto_label = match resolve_provider(ProviderArg::Auto) {
        EmbedProvider::Cpu => "CPU",
        EmbedProvider::Cuda => "CUDA",
        // Unreachable today — `auto` never picks CoreML (plan-30) — but
        // kept so the match stays exhaustive over `EmbedProvider` without
        // a `_` arm that would silently mislabel a future variant.
        EmbedProvider::CoreMl => "CoreML",
    };
    ProviderAvailability {
        macos: cfg!(target_os = "macos"),
        coreml_build: cfg!(feature = "coreml"),
        cuda_build: cfg!(feature = "cuda"),
        auto_label,
    }
}

/// Build the provider `Select` list for the wizard. Returns
/// `(choices, labels, footnotes)`: `choices[i]` is what selecting
/// `labels[i]` means; `footnotes` explain any provider hidden because
/// this build can't run it. `Auto` is always index 0, `Cpu` index 1.
pub fn provider_choices(
    a: &ProviderAvailability,
) -> (Vec<ProviderChoice>, Vec<String>, Vec<String>) {
    let mut choices = vec![ProviderChoice::Auto, ProviderChoice::Cpu];
    let mut labels = vec![
        format!(
            "Auto (recommended) — resolves to {} on this host",
            a.auto_label
        ),
        "CPU".to_string(),
    ];
    let mut footnotes = Vec::new();

    match (a.coreml_offerable(), a.macos) {
        (true, _) => {
            choices.push(ProviderChoice::Coreml);
            labels.push(
                "CoreML — ~3x faster on Apple Silicon; first run downloads \
                 ~130MB and pays a one-time ~30s compile"
                    .to_string(),
            );
        }
        (false, true) => footnotes
            .push("CoreML hidden: this binary was built without `--features coreml`.".to_string()),
        (false, false) => {}
    }

    match (a.cuda_build, a.macos) {
        (true, _) => {
            choices.push(ProviderChoice::Cuda);
            labels.push("CUDA — NVIDIA GPU".to_string());
        }
        (false, false) => footnotes
            .push("CUDA hidden: this binary was built without `--features cuda`.".to_string()),
        (false, true) => {}
    }

    (choices, labels, footnotes)
}

#[cfg(test)]
mod provider_choice_tests {
    use super::*;

    fn avail(macos: bool, coreml_build: bool, cuda_build: bool) -> ProviderAvailability {
        ProviderAvailability {
            macos,
            coreml_build,
            cuda_build,
            auto_label: "CPU",
        }
    }

    #[test]
    fn auto_and_cpu_are_always_offered_first() {
        let (choices, labels, _) = provider_choices(&avail(false, false, false));
        assert_eq!(choices[0], ProviderChoice::Auto);
        assert_eq!(choices[1], ProviderChoice::Cpu);
        assert!(labels[0].contains("Auto"));
        assert!(labels[0].contains("CPU")); // avail() sets auto_label = "CPU"
    }

    #[test]
    fn coreml_offered_only_on_macos_with_feature() {
        let (with, _, _) = provider_choices(&avail(true, true, false));
        assert!(with.contains(&ProviderChoice::Coreml));

        let (without_feat, _, foot) = provider_choices(&avail(true, false, false));
        assert!(!without_feat.contains(&ProviderChoice::Coreml));
        assert!(foot.iter().any(|f| f.contains("CoreML")));
    }

    #[test]
    fn coreml_not_footnoted_off_macos() {
        let (_, _, foot) = provider_choices(&avail(false, false, false));
        assert!(!foot.iter().any(|f| f.contains("CoreML")));
    }

    #[test]
    fn cuda_offered_only_with_feature() {
        let (with, _, _) = provider_choices(&avail(false, false, true));
        assert!(with.contains(&ProviderChoice::Cuda));

        let (without, _, foot) = provider_choices(&avail(false, false, false));
        assert!(!without.contains(&ProviderChoice::Cuda));
        assert!(foot.iter().any(|f| f.contains("CUDA")));
    }

    #[test]
    fn host_capabilities_reports_known_auto_label() {
        let a = host_capabilities();
        assert!(matches!(a.auto_label, "CPU" | "CUDA" | "CoreML"));
        assert_eq!(a.coreml_build, cfg!(feature = "coreml"));
        assert_eq!(a.cuda_build, cfg!(feature = "cuda"));
    }

    #[test]
    fn cuda_not_footnoted_on_macos_without_feature() {
        let (choices, _, foot) = provider_choices(&avail(true, false, false));
        assert!(!choices.contains(&ProviderChoice::Cuda));
        assert!(!foot.iter().any(|f| f.contains("CUDA")));
    }
}

fn provider_arg_str(p: ProviderArg) -> &'static str {
    match p {
        ProviderArg::Auto => "auto",
        ProviderArg::Cpu => "cpu",
        ProviderArg::Coreml => "coreml",
        ProviderArg::Cuda => "cuda",
    }
}

fn resources_str(r: ResourcesArg) -> &'static str {
    match r {
        ResourcesArg::Auto => "auto",
        ResourcesArg::Conservative => "conservative",
        ResourcesArg::Aggressive => "aggressive",
    }
}

fn embed_cache_str(c: EmbedCacheArg) -> &'static str {
    match c {
        EmbedCacheArg::Off => "off",
        EmbedCacheArg::Semantic => "semantic",
        EmbedCacheArg::Diff => "diff",
    }
}

/// Render the equivalent `ohara index …` command for an assembled
/// `Args`, omitting every flag left at its default so the line shows
/// only what the wizard chose. Used in the summary the user confirms.
pub fn args_to_command(a: &Args) -> String {
    let mut parts: Vec<String> = vec!["ohara".to_string(), "index".to_string()];

    if let Some(p) = a.embed_provider {
        parts.push("--embed-provider".to_string());
        parts.push(provider_arg_str(p).to_string());
    }
    if a.resources != ResourcesArg::Auto {
        parts.push("--resources".to_string());
        parts.push(resources_str(a.resources).to_string());
    }
    if a.incremental {
        parts.push("--incremental".to_string());
    }
    if a.force {
        parts.push("--force".to_string());
    }
    if a.rebuild {
        parts.push("--rebuild".to_string());
        parts.push("--yes".to_string());
    }
    if let Some(n) = a.commit_batch {
        parts.push("--commit-batch".to_string());
        parts.push(n.to_string());
    }
    if let Some(n) = a.embed_batch {
        parts.push("--embed-batch".to_string());
        parts.push(n.to_string());
    }
    if let Some(n) = a.threads {
        parts.push("--threads".to_string());
        parts.push(n.to_string());
    }
    if let Some(n) = a.workers {
        parts.push("--workers".to_string());
        parts.push(n.to_string());
    }
    if a.embed_cache != EmbedCacheArg::Off {
        parts.push("--embed-cache".to_string());
        parts.push(embed_cache_str(a.embed_cache).to_string());
    }
    if a.no_progress {
        parts.push("--no-progress".to_string());
    }
    if a.profile {
        parts.push("--profile".to_string());
    }

    let path = a.path.to_string_lossy();
    if path != "." {
        parts.push(path.into_owned());
    }

    parts.join(" ")
}

#[cfg(test)]
mod command_render_tests {
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
    fn defaults_render_bare_command() {
        let a = assemble_args(WizardAnswers::default(), base_args());
        assert_eq!(args_to_command(&a), "ohara index");
    }

    #[test]
    fn cpu_aggressive_renders_both_flags() {
        let ans = WizardAnswers {
            provider: ProviderChoice::Cpu,
            intensity: ResourcesArg::Aggressive,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(
            args_to_command(&a),
            "ohara index --embed-provider cpu --resources aggressive"
        );
    }

    #[test]
    fn rebuild_renders_rebuild_yes() {
        let ans = WizardAnswers {
            mode: ModeChoice::Rebuild,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(args_to_command(&a), "ohara index --rebuild --yes");
    }

    #[test]
    fn advanced_knobs_render_in_order() {
        let ans = WizardAnswers {
            provider: ProviderChoice::Coreml,
            mode: ModeChoice::Force,
            threads: Some(4),
            workers: Some(2),
            commit_batch: Some(512),
            embed_batch: Some(256),
            embed_cache: EmbedCacheArg::Diff,
            no_progress: true,
            profile: true,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(
            args_to_command(&a),
            "ohara index --embed-provider coreml --force \
             --commit-batch 512 --embed-batch 256 --threads 4 --workers 2 \
             --embed-cache diff --no-progress --profile"
        );
    }

    #[test]
    fn non_dot_path_is_appended() {
        let mut a = assemble_args(WizardAnswers::default(), base_args());
        a.path = std::path::PathBuf::from("/repo/x");
        assert_eq!(args_to_command(&a), "ohara index /repo/x");
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
        let ans = WizardAnswers {
            mode: ModeChoice::Incremental,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert!(a.incremental && !a.force && !a.rebuild);
    }

    #[test]
    fn force_mode_sets_force_only() {
        let ans = WizardAnswers {
            mode: ModeChoice::Force,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert!(a.force && !a.incremental && !a.rebuild);
    }

    #[test]
    fn rebuild_mode_sets_rebuild_and_yes() {
        let ans = WizardAnswers {
            mode: ModeChoice::Rebuild,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert!(a.rebuild && a.yes);
        assert!(!a.incremental && !a.force);
    }

    #[test]
    fn provider_auto_maps_to_none() {
        let ans = WizardAnswers {
            provider: ProviderChoice::Auto,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(a.embed_provider, None);
    }

    #[test]
    fn provider_explicit_maps_to_some() {
        let ans = WizardAnswers {
            provider: ProviderChoice::Coreml,
            ..Default::default()
        };
        let a = assemble_args(ans, base_args());
        assert_eq!(a.embed_provider, Some(ProviderArg::Coreml));

        assert_eq!(
            ProviderChoice::Cpu.to_provider_arg(),
            Some(ProviderArg::Cpu)
        );
        assert_eq!(
            ProviderChoice::Cuda.to_provider_arg(),
            Some(ProviderArg::Cuda)
        );
        assert_eq!(ProviderChoice::Auto.to_provider_arg(), None);
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
