//! Interactive `ohara index -i` wizard.
//!
//! A pure front-end over `commands::index::run`: it collects choices
//! through the [`WizardPrompter`] trait and assembles an
//! [`crate::commands::index::Args`]. The answer→Args mapping, provider
//! availability, and command rendering live in `assemble` and are
//! TTY-free / unit-tested.

use anyhow::Result;

use crate::commands::index::{Args, EmbedCacheArg};
use crate::resources::ResourcesArg;

mod assemble;
pub use assemble::*;

/// Outcome of a wizard session, returned to `index::run`.
pub enum WizardFlow {
    /// Run the index with these assembled args.
    Run(Args),
    /// User declined to run; this is the equivalent command to print.
    PrintOnly(String),
    /// User aborted (ESC / Ctrl-C); index nothing, exit cleanly.
    Cancelled,
}

/// The wizard's only contact with the terminal. The real impl wraps
/// `dialoguer`; tests inject a scripted fake. Keeping every prompt
/// behind this trait makes the whole flow runnable without a TTY.
pub trait WizardPrompter {
    /// Print an informational line (footnotes, the equivalent command).
    fn note(&mut self, msg: &str);
    /// Single-choice select; returns the chosen index into `options`.
    fn select(&mut self, prompt: &str, options: &[String], default: usize) -> Result<usize>;
    /// Yes/no confirm.
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool>;
    /// Free-text input (may be empty).
    fn input(&mut self, prompt: &str) -> Result<String>;
}

/// Prompt for an optional `usize`: blank or `auto` → `None`; a valid
/// number → `Some(n)`; anything else re-prompts (validation lives here,
/// not in the prompter, so the trait stays minimal and the loop is
/// testable with a scripted bad-then-good sequence).
fn prompt_opt_usize(p: &mut dyn WizardPrompter, label: &str) -> Result<Option<usize>> {
    loop {
        let raw = p.input(&format!("{label} (blank or 'auto' for default)"))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
            return Ok(None);
        }
        match trimmed.parse::<usize>() {
            Ok(n) => return Ok(Some(n)),
            Err(_) => p.note(&format!("'{trimmed}' is not a whole number — try again.")),
        }
    }
}

/// Drive the wizard through `p`, returning what to do next. Pure with
/// respect to I/O: every read/write goes through `p`.
pub fn run_wizard_with(p: &mut dyn WizardPrompter, base: Args) -> Result<WizardFlow> {
    // 1. Embedding provider.
    let avail = host_capabilities();
    let (choices, labels, footnotes) = provider_choices(&avail);
    for f in &footnotes {
        p.note(f);
    }
    let pi = p.select("Embedding provider", &labels, 0)?;
    let provider = choices[pi];

    // 2. Resource intensity.
    let intensity_opts = vec![
        "Auto (recommended)".to_string(),
        "Conservative — halve batch + threads, yield to other work".to_string(),
        "Aggressive — double batch + threads, maximize throughput".to_string(),
    ];
    let ii = p.select("Resource intensity", &intensity_opts, 0)?;
    let intensity = [
        ResourcesArg::Auto,
        ResourcesArg::Conservative,
        ResourcesArg::Aggressive,
    ][ii];

    // 3. Index mode. Rebuild gets a destructive confirm; declining it
    // falls back to Standard.
    let mode_opts = vec![
        "Standard — index new commits".to_string(),
        "Incremental — skip entirely if HEAD already indexed".to_string(),
        "Force — re-extract HEAD symbols (after a chunker change)".to_string(),
        "Rebuild — delete & rebuild the whole index from scratch".to_string(),
    ];
    let mi = p.select("Index mode", &mode_opts, 0)?;
    let mut mode = [
        ModeChoice::Standard,
        ModeChoice::Incremental,
        ModeChoice::Force,
        ModeChoice::Rebuild,
    ][mi];
    if matches!(mode, ModeChoice::Rebuild) {
        let confirmed = p.confirm(
            &format!(
                "Rebuild deletes and recreates the entire index for {}. Continue?",
                base.path.display()
            ),
            false,
        )?;
        if !confirmed {
            mode = ModeChoice::Standard;
        }
    }

    // 4. Advanced knobs, opt-in.
    let mut ans = WizardAnswers {
        provider,
        intensity,
        mode,
        ..Default::default()
    };
    if p.confirm("Configure advanced knobs?", false)? {
        ans.threads = prompt_opt_usize(p, "threads")?;
        ans.workers = prompt_opt_usize(p, "workers")?;
        ans.commit_batch = prompt_opt_usize(p, "commit-batch")?;
        ans.embed_batch = prompt_opt_usize(p, "embed-batch")?;
        let cache_opts = vec![
            "off".to_string(),
            "semantic".to_string(),
            "diff".to_string(),
        ];
        let ci = p.select("Embed cache mode", &cache_opts, 0)?;
        ans.embed_cache = [EmbedCacheArg::Off, EmbedCacheArg::Semantic, EmbedCacheArg::Diff][ci];
        ans.no_progress = p.confirm("Disable the progress bar?", false)?;
        ans.profile = p.confirm("Emit per-phase --profile JSON?", false)?;
    }

    // 5. Summary + run.
    let args = assemble_args(ans, base);
    let command = args_to_command(&args);
    p.note(&format!("Equivalent command:\n  {command}"));
    if p.confirm("Run now?", true)? {
        return Ok(WizardFlow::Run(args));
    }
    Ok(WizardFlow::PrintOnly(command))
}

#[cfg(test)]
mod orchestration_tests {
    use super::*;
    use crate::commands::provider::ProviderArg;
    use clap::Parser;
    use std::collections::VecDeque;

    fn base_args() -> Args {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: Args,
        }
        Wrapper::parse_from(["ohara"]).args
    }

    /// Replays canned answers in order; panics on an unexpected prompt
    /// so a drift in prompt count is caught.
    struct Scripted {
        selects: VecDeque<usize>,
        confirms: VecDeque<bool>,
        inputs: VecDeque<String>,
        notes: Vec<String>,
    }
    impl Scripted {
        fn new() -> Self {
            Self {
                selects: VecDeque::new(),
                confirms: VecDeque::new(),
                inputs: VecDeque::new(),
                notes: Vec::new(),
            }
        }
    }
    impl WizardPrompter for Scripted {
        fn note(&mut self, msg: &str) {
            self.notes.push(msg.to_string());
        }
        fn select(&mut self, _prompt: &str, _options: &[String], _default: usize) -> Result<usize> {
            Ok(self.selects.pop_front().expect("unexpected select"))
        }
        fn confirm(&mut self, _prompt: &str, _default: bool) -> Result<bool> {
            Ok(self.confirms.pop_front().expect("unexpected confirm"))
        }
        fn input(&mut self, _prompt: &str) -> Result<String> {
            Ok(self.inputs.pop_front().expect("unexpected input"))
        }
    }

    #[test]
    fn happy_path_cpu_aggressive_standard_runs() {
        let mut s = Scripted::new();
        s.selects.push_back(1); // provider = Cpu (Auto=0, Cpu=1 always)
        s.selects.push_back(2); // intensity = Aggressive
        s.selects.push_back(0); // mode = Standard
        s.confirms.push_back(false); // advanced? no
        s.confirms.push_back(true); // run now? yes
        match run_wizard_with(&mut s, base_args()).expect("wizard ok") {
            WizardFlow::Run(a) => {
                assert_eq!(a.embed_provider, Some(ProviderArg::Cpu));
                assert_eq!(a.resources, ResourcesArg::Aggressive);
                assert!(!a.incremental && !a.force && !a.rebuild);
                assert!(!a.interactive);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn decline_run_returns_print_only_bare_command() {
        let mut s = Scripted::new();
        s.selects.push_back(0); // provider = Auto
        s.selects.push_back(0); // intensity = Auto
        s.selects.push_back(0); // mode = Standard
        s.confirms.push_back(false); // advanced? no
        s.confirms.push_back(false); // run now? no
        match run_wizard_with(&mut s, base_args()).expect("ok") {
            WizardFlow::PrintOnly(cmd) => assert_eq!(cmd, "ohara index"),
            _ => panic!("expected PrintOnly"),
        }
    }

    #[test]
    fn rebuild_declined_falls_back_to_standard() {
        let mut s = Scripted::new();
        s.selects.push_back(0); // provider Auto
        s.selects.push_back(0); // intensity Auto
        s.selects.push_back(3); // mode = Rebuild
        s.confirms.push_back(false); // rebuild confirm -> no
        s.confirms.push_back(false); // advanced? no
        s.confirms.push_back(true); // run now? yes
        match run_wizard_with(&mut s, base_args()).expect("ok") {
            WizardFlow::Run(a) => assert!(!a.rebuild && !a.yes),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn advanced_collects_numeric_and_cache() {
        let mut s = Scripted::new();
        s.selects.push_back(1); // provider Cpu
        s.selects.push_back(0); // intensity Auto
        s.selects.push_back(0); // mode Standard
        s.confirms.push_back(true); // advanced? yes
        s.inputs.push_back("4".to_string()); // threads
        s.inputs.push_back("auto".to_string()); // workers -> None
        s.inputs.push_back(String::new()); // commit-batch -> None
        s.inputs.push_back("256".to_string()); // embed-batch
        s.selects.push_back(2); // embed cache = diff
        s.confirms.push_back(true); // no-progress yes
        s.confirms.push_back(false); // profile no
        s.confirms.push_back(true); // run yes
        match run_wizard_with(&mut s, base_args()).expect("ok") {
            WizardFlow::Run(a) => {
                assert_eq!(a.threads, Some(4));
                assert_eq!(a.workers, None);
                assert_eq!(a.commit_batch, None);
                assert_eq!(a.embed_batch, Some(256));
                assert_eq!(a.embed_cache, EmbedCacheArg::Diff);
                assert!(a.no_progress && !a.profile);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn numeric_input_reprompts_on_garbage() {
        let mut s = Scripted::new();
        s.inputs.push_back("notanumber".to_string());
        s.inputs.push_back("12".to_string());
        let v = prompt_opt_usize(&mut s, "threads").expect("ok");
        assert_eq!(v, Some(12));
        assert!(s.notes.iter().any(|n| n.contains("not a whole number")));
    }
}
