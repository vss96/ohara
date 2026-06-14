# ohara index interactive wizard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ohara index -i` / `--interactive` — a sequential prompt wizard that walks the user through embedding provider, resource intensity, index mode, and (opt-in) advanced knobs, shows the equivalent CLI command, then runs the existing index path.

**Architecture:** The wizard is a pure front-end. It collects answers through a small `WizardPrompter` trait (real impl: `dialoguer`; test impl: a scripted fake), assembles a `commands::index::Args`, and hands it to the unchanged `commands::index::run`. All answer→`Args` mapping, provider-availability logic, and command rendering are pure, TTY-free functions with unit tests. Only the thin `DialoguerPrompter` and the `run_wizard_tty` (TTY guard + `spawn_blocking`) wrapper touch the terminal.

**Tech Stack:** Rust, `clap` (derive), `dialoguer` (console-rs family, shares `console` with the existing `indicatif`), `tokio` (`spawn_blocking`).

**Spec:** `docs/superpowers/specs/2026-06-14-ohara-index-interactive-wizard-design.md`

---

## File structure

| File | Responsibility |
|---|---|
| `crates/ohara-cli/src/commands/index.rs` (modify) | Add `interactive` flag to `Args`; wizard branch + `noop_report()` at top of `run`. |
| `crates/ohara-cli/src/commands/mod.rs` (modify) | Register `pub mod index_wizard;`. |
| `crates/ohara-cli/src/commands/index_wizard/mod.rs` (create) | `WizardPrompter` trait, `WizardFlow`, `run_wizard_with` orchestration, `prompt_opt_usize`, `DialoguerPrompter`, `require_tty`, `run_wizard_tty`. |
| `crates/ohara-cli/src/commands/index_wizard/assemble.rs` (create) | `ProviderChoice`, `ModeChoice`, `WizardAnswers`, `assemble_args`, `ProviderAvailability`, `host_capabilities`, `provider_choices`, `args_to_command`, string helpers. |
| `Cargo.toml` (modify) | Add `dialoguer` to `[workspace.dependencies]`. |
| `crates/ohara-cli/Cargo.toml` (modify) | `dialoguer.workspace = true`. |
| `docs-book/src/cli/index.md` (modify) | Document `-i` / `--interactive`. |

Splitting into `index_wizard/{mod.rs, assemble.rs}` keeps each file focused and under the 500-line cap (CONTRIBUTING §). `assemble.rs` holds the pure logic + its tests; `mod.rs` holds orchestration + the I/O boundary + their tests.

---

## Task 1: Add the `--interactive` flag to `index::Args`

**Files:**
- Modify: `crates/ohara-cli/src/commands/index.rs` (the `Args` struct, ~line 28-115)
- Test: same file (`#[cfg(test)] mod interactive_flag_tests`)

- [ ] **Step 1: Write the failing test**

Add at the end of `crates/ohara-cli/src/commands/index.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ohara-cli interactive_flag_tests`
Expected: FAIL — compile error, `Args` has no field `interactive`.

- [ ] **Step 3: Add the field**

In `crates/ohara-cli/src/commands/index.rs`, inside `pub struct Args`, immediately after the `path` field (around line 32), add:

```rust
    /// Launch the interactive index wizard: choose embedding provider,
    /// resource intensity, index mode, and advanced knobs through
    /// guided prompts, preview the equivalent command, then run.
    /// Requires a TTY. Other tuning flags are ignored when `-i` is set
    /// — the wizard owns the tuning surface.
    #[arg(long, short = 'i')]
    pub interactive: bool,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ohara-cli interactive_flag_tests`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/index.rs
git commit -m "feat(cli): add --interactive/-i flag to index Args"
```

---

## Task 2: `WizardAnswers` + pure `assemble_args`

**Files:**
- Modify: `crates/ohara-cli/src/commands/index.rs` (add `Default` to `EmbedCacheArg`)
- Modify: `crates/ohara-cli/src/commands/mod.rs` (register module)
- Create: `crates/ohara-cli/src/commands/index_wizard/mod.rs`
- Create: `crates/ohara-cli/src/commands/index_wizard/assemble.rs`
- Test: in `assemble.rs`

- [ ] **Step 1: Make `EmbedCacheArg` derive `Default`**

In `crates/ohara-cli/src/commands/index.rs`, change the `EmbedCacheArg` definition (around line 11-16) from:

```rust
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum EmbedCacheArg {
    Off,
    Semantic,
    Diff,
}
```

to:

```rust
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum EmbedCacheArg {
    #[default]
    Off,
    Semantic,
    Diff,
}
```

- [ ] **Step 2: Register the module**

In `crates/ohara-cli/src/commands/mod.rs`, add to the `pub mod` block (alphabetical, after `pub mod index;`):

```rust
pub mod index_wizard;
```

- [ ] **Step 3: Create the module entrypoint**

Create `crates/ohara-cli/src/commands/index_wizard/mod.rs` with just the submodule wiring for now:

```rust
//! Interactive `ohara index -i` wizard.
//!
//! A pure front-end over `commands::index::run`: it collects choices
//! through the [`WizardPrompter`] trait and assembles an
//! [`crate::commands::index::Args`]. The answer→Args mapping, provider
//! availability, and command rendering live in `assemble` and are
//! TTY-free / unit-tested.

mod assemble;
pub use assemble::*;
```

- [ ] **Step 4: Write the failing test**

Create `crates/ohara-cli/src/commands/index_wizard/assemble.rs`:

```rust
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

// `EmbedProvider` / `resolve_provider` are used by host_capabilities in
// a later task; the `use` is kept here so that task is a pure add.
#[allow(unused_imports)]
use self::keep_imports_live as _keep;
fn keep_imports_live() {
    let _ = resolve_provider;
    let _: Option<EmbedProvider> = None;
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
        // Rebuild MUST carry --yes; the index path refuses --rebuild
        // without it.
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
```

Note: `assemble.rs` here imports only `Args`, `EmbedCacheArg`,
`ProviderArg`, and `ResourcesArg` — exactly what `assemble_args` and the
answer types use. Task 3 adds the `resolve_provider` / `EmbedProvider`
imports when `host_capabilities` starts using them, so there are no
unused-import warnings at any task boundary.

- [ ] **Step 5: Run test to verify it fails, then passes**

Run: `cargo test -p ohara-cli assemble_tests`
Expected: PASS after the file compiles (the test file *is* the implementation here — it should pass once it compiles). If it fails to compile, fix imports/field names until green.

- [ ] **Step 6: Commit**

```bash
git add crates/ohara-cli/src/commands/index.rs crates/ohara-cli/src/commands/mod.rs crates/ohara-cli/src/commands/index_wizard/
git commit -m "feat(cli): wizard answer types and pure assemble_args"
```

---

## Task 3: provider availability + `provider_choices`

**Files:**
- Modify: `crates/ohara-cli/src/commands/index_wizard/assemble.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

Add a new test module to the end of `assemble.rs`:

```rust
#[cfg(test)]
mod provider_choice_tests {
    use super::*;

    fn avail(macos: bool, coreml_build: bool, cuda_build: bool) -> ProviderAvailability {
        ProviderAvailability { macos, coreml_build, cuda_build, auto_label: "CPU" }
    }

    #[test]
    fn auto_and_cpu_are_always_offered_first() {
        let (choices, labels, _) = provider_choices(&avail(false, false, false));
        assert_eq!(choices[0], ProviderChoice::Auto);
        assert_eq!(choices[1], ProviderChoice::Cpu);
        assert!(labels[0].contains("Auto"));
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
        // On Linux, a missing CoreML is irrelevant — no footnote noise.
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ohara-cli provider_choice_tests`
Expected: FAIL — `ProviderAvailability`, `provider_choices`, `host_capabilities` not defined.

- [ ] **Step 3: Implement**

In `assemble.rs`, first extend the imports at the top of the file so
`host_capabilities` can resolve the auto provider. Change:

```rust
use crate::commands::provider::ProviderArg;
```

to:

```rust
use crate::commands::provider::{resolve_provider, ProviderArg};
use ohara_embed::EmbedProvider;
```

Then add:

```rust
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
    fn cuda_offerable(&self) -> bool {
        self.cuda_build
    }
}

/// Detect provider availability from build features + the same
/// auto-resolution the index path uses.
pub fn host_capabilities() -> ProviderAvailability {
    let auto_label = match resolve_provider(ProviderArg::Auto) {
        EmbedProvider::Cpu => "CPU",
        EmbedProvider::Cuda => "CUDA",
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
        format!("Auto (recommended) — resolves to {} on this host", a.auto_label),
        "CPU".to_string(),
    ];
    let mut footnotes = Vec::new();

    if a.coreml_offerable() {
        choices.push(ProviderChoice::Coreml);
        labels.push(
            "CoreML — ~3x faster on Apple Silicon; first run downloads \
             ~130MB and pays a one-time ~30s compile"
                .to_string(),
        );
    } else if a.macos {
        footnotes.push(
            "CoreML hidden: this binary was built without `--features coreml`.".to_string(),
        );
    }

    if a.cuda_offerable() {
        choices.push(ProviderChoice::Cuda);
        labels.push("CUDA — NVIDIA GPU".to_string());
    } else if !a.macos {
        footnotes
            .push("CUDA hidden: this binary was built without `--features cuda`.".to_string());
    }

    (choices, labels, footnotes)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ohara-cli provider_choice_tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/index_wizard/assemble.rs
git commit -m "feat(cli): host/build-aware provider_choices for wizard"
```

---

## Task 4: `args_to_command` renderer

**Files:**
- Modify: `crates/ohara-cli/src/commands/index_wizard/assemble.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

Add to `assemble.rs`:

```rust
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
        let ans = WizardAnswers { mode: ModeChoice::Rebuild, ..Default::default() };
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
```

Note: the `\` line continuations inside the `assert_eq!` strings collapse the following line's leading whitespace into a single run of spaces — written so the expected string has exactly single spaces between tokens. When implementing, double-check there are no doubled spaces.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ohara-cli command_render_tests`
Expected: FAIL — `args_to_command` not defined.

- [ ] **Step 3: Implement**

Add to `assemble.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ohara-cli command_render_tests`
Expected: PASS (5 tests). If a string mismatch appears, it is almost always a doubled space from the line-continuation — collapse to single spaces in the expected literal.

- [ ] **Step 5: Commit**

```bash
git add crates/ohara-cli/src/commands/index_wizard/assemble.rs
git commit -m "feat(cli): args_to_command renderer for wizard summary"
```

---

## Task 5: `WizardPrompter` trait + `run_wizard_with` orchestration

**Files:**
- Modify: `crates/ohara-cli/src/commands/index_wizard/mod.rs`
- Test: same file (with a scripted fake prompter)

- [ ] **Step 1: Write the orchestration + trait (implementation first, tests next step)**

Replace the contents of `crates/ohara-cli/src/commands/index_wizard/mod.rs` with:

```rust
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
        Ok(WizardFlow::Run(args))
    } else {
        Ok(WizardFlow::PrintOnly(command))
    }
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
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p ohara-cli orchestration_tests`
Expected: PASS (5 tests).

- [ ] **Step 3: Commit**

```bash
git add crates/ohara-cli/src/commands/index_wizard/mod.rs
git commit -m "feat(cli): WizardPrompter trait and run_wizard_with orchestration"
```

---

## Task 6: `dialoguer` impl, TTY guard, and wiring into `index::run`

**Files:**
- Modify: `Cargo.toml` (workspace deps)
- Modify: `crates/ohara-cli/Cargo.toml`
- Modify: `crates/ohara-cli/src/commands/index_wizard/mod.rs` (add `DialoguerPrompter`, `require_tty`, `run_wizard_tty`)
- Modify: `crates/ohara-cli/src/commands/index.rs` (`run` wizard branch + `noop_report`)
- Test: `mod.rs` (TTY guard)

- [ ] **Step 1: Add the dependency**

In root `Cargo.toml`, under `[workspace.dependencies]` (alphabetical, near `clap`), add:

```toml
dialoguer = { version = "0.11", default-features = false }
```

In `crates/ohara-cli/Cargo.toml`, under `[dependencies]` (alphabetical, after `clap.workspace = true`), add:

```toml
dialoguer.workspace = true
```

- [ ] **Step 2: Verify it resolves**

Run: `cargo build -p ohara-cli`
Expected: builds clean. If cargo reports a `console` version conflict, change the version to one whose `console` matches the lockfile's existing `0.15.x` (the family `indicatif` already uses) and re-run.

- [ ] **Step 3: Write the TTY-guard test**

Append to the `orchestration_tests` module in `mod.rs` (or add a new `tty_tests` module):

```rust
#[cfg(test)]
mod tty_tests {
    use super::*;

    #[test]
    fn tty_present_is_ok() {
        assert!(require_tty(true).is_ok());
    }

    #[test]
    fn non_tty_errors_with_actionable_message() {
        let err = require_tty(false).expect_err("non-tty must error");
        let s = err.to_string();
        assert!(s.contains("TTY"), "message should name the constraint: {s}");
        assert!(s.contains("--interactive"), "message should name the flag: {s}");
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p ohara-cli tty_tests`
Expected: FAIL — `require_tty` not defined.

- [ ] **Step 5: Implement the dialoguer impl + TTY wrapper**

Add to `crates/ohara-cli/src/commands/index_wizard/mod.rs` (after `run_wizard_with`, before the test modules):

```rust
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

/// The real terminal prompter, backed by `dialoguer`.
pub struct DialoguerPrompter {
    theme: ColorfulTheme,
}

impl DialoguerPrompter {
    pub fn new() -> Self {
        Self {
            theme: ColorfulTheme::default(),
        }
    }
}

impl Default for DialoguerPrompter {
    fn default() -> Self {
        Self::new()
    }
}

impl WizardPrompter for DialoguerPrompter {
    fn note(&mut self, msg: &str) {
        // Wizard chrome goes to stderr so a piped stdout stays clean.
        eprintln!("{msg}");
    }
    fn select(&mut self, prompt: &str, options: &[String], default: usize) -> Result<usize> {
        Select::with_theme(&self.theme)
            .with_prompt(prompt)
            .items(options)
            .default(default)
            .interact()
            .map_err(|e| anyhow::anyhow!(e))
    }
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        Confirm::with_theme(&self.theme)
            .with_prompt(prompt)
            .default(default)
            .interact()
            .map_err(|e| anyhow::anyhow!(e))
    }
    fn input(&mut self, prompt: &str) -> Result<String> {
        Input::<String>::with_theme(&self.theme)
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Guard: the wizard needs an attended terminal. Split out so the
/// branch is unit-testable without a real TTY.
fn require_tty(is_tty: bool) -> Result<()> {
    if is_tty {
        return Ok(());
    }
    anyhow::bail!(
        "--interactive requires a TTY; pass explicit flags \
         (e.g. --embed-provider cpu) instead"
    )
}

/// Run the wizard against the real terminal. Checks for a TTY, then
/// runs the (blocking) `dialoguer` prompts on a blocking thread so the
/// async runtime is not stalled. Any error from the prompts (ESC /
/// Ctrl-C) is treated as a clean cancel.
pub async fn run_wizard_tty(base: Args) -> Result<WizardFlow> {
    use std::io::IsTerminal;
    require_tty(std::io::stdin().is_terminal())?;
    let joined = tokio::task::spawn_blocking(move || {
        let mut prompter = DialoguerPrompter::new();
        run_wizard_with(&mut prompter, base)
    })
    .await?;
    match joined {
        Ok(flow) => Ok(flow),
        Err(_) => Ok(WizardFlow::Cancelled),
    }
}
```

- [ ] **Step 6: Run the TTY test to verify it passes**

Run: `cargo test -p ohara-cli tty_tests`
Expected: PASS (2 tests).

- [ ] **Step 7: Wire the wizard into `index::run`**

In `crates/ohara-cli/src/commands/index.rs`, change the start of `pub async fn run` from:

```rust
pub async fn run(args: Args) -> Result<IndexerReport> {
    // Wall-clock starts at the very top so the summary's `total_ms`
```

to:

```rust
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
```

Then add a `noop_report` helper near the other free functions in `index.rs` (e.g. just above `pub async fn run`):

```rust
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
```

Finally, DRY up the existing incremental fast-path: in `run`, replace the inline `return Ok(IndexerReport { … })` block (the one under `if st.last_indexed_commit.as_deref() == Some(head.as_str())`) with:

```rust
            println!("index up-to-date at {head}");
            return Ok(noop_report());
```

- [ ] **Step 8: Build and run the full crate test suite**

Run: `cargo test -p ohara-cli`
Expected: PASS — all wizard tests plus the pre-existing `index.rs` tests stay green.

- [ ] **Step 9: Manual smoke test (TTY required, not automated)**

Run: `cargo run -p ohara-cli -- index -i fixtures/tiny/repo`
(Build the fixture first if needed: `fixtures/build_tiny.sh`.)
Expected: provider → intensity → mode → "Configure advanced knobs?" prompts appear; choosing CPU / Auto / Standard / no / "Run now? yes" indexes the fixture and prints the usual summary. Declining "Run now?" prints `ohara index` and exits without indexing. Piping (`... index -i < /dev/null`) prints the non-TTY error and exits non-zero.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml crates/ohara-cli/Cargo.toml crates/ohara-cli/src/commands/index_wizard/mod.rs crates/ohara-cli/src/commands/index.rs
git commit -m "feat(cli): wire interactive wizard into ohara index -i"
```

---

## Task 7: Docs + final verification

**Files:**
- Modify: `docs-book/src/cli/index.md`

- [ ] **Step 1: Document the flag in the usage block**

In `docs-book/src/cli/index.md`, change the `## Usage` code block to include the interactive flag:

```
ohara index [PATH] [-i | --interactive] [--incremental] [--force] \
            [--rebuild --yes] [--commit-batch N] [--threads N] \
            [--no-progress] [--profile] \
            [--embed-provider {auto,cpu,coreml,cuda}] \
            [--resources {auto,conservative,aggressive}]
```

- [ ] **Step 2: Add a table row**

In the flag table, add this row immediately under the `PATH` row:

```
| `-i`, `--interactive` | off | Launch a guided wizard that prompts for embedding provider, resource intensity, index mode, and (opt-in) advanced knobs, previews the equivalent command, then runs it. Requires a TTY. Other tuning flags are ignored when `-i` is set — the wizard owns the tuning. |
```

- [ ] **Step 3: Add an example**

In the `## Examples` section, add after the first ("First-time index") example:

````
Not sure which provider or knobs to use? Launch the interactive wizard:

```sh
ohara index -i
```

It walks you through provider (CoreML/CPU/CUDA — only the ones this
build supports), resource intensity, and index mode, shows the
equivalent `ohara index …` command, and runs it on confirm.
````

- [ ] **Step 4: Full workspace verification (CONTRIBUTING §13)**

Run each and confirm clean:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: `fmt` produces no diff; `clippy` exits 0 with no warnings; all tests pass. (If `--all-features` fails to build locally because the `cuda` feature needs CUDA libs, re-run clippy/test without `--all-features` and note it — CI runs the full matrix.)

- [ ] **Step 5: Commit**

```bash
git add docs-book/src/cli/index.md
git commit -m "docs: document ohara index -i interactive wizard"
```

---

## Self-review notes

- **Spec coverage:** trigger (`-i`/`--interactive`, Task 1) · `dialoguer` (Task 6) · 3-prompt happy path + advanced opt-in (Task 5) · host/build-aware provider filtering + footnotes (Task 3) · rebuild double-confirm (Task 5) · equivalent-command summary (Task 4 render, Task 5 display) · `WizardPrompter` + pure `assemble_args`/`host_capabilities`/`args_to_command` (Tasks 2-4) · non-TTY guard + ESC cancel (Task 6) · docs (Task 7). All spec sections map to a task.
- **Embed-cache mismatch guard:** intentionally untouched — the existing guard in `index::run` still fires for an incompatible `--embed-cache` on an existing index (spec "optional nicety" deferred; not a v1 requirement).
- **`-i` + other flags:** `assemble_args` reads only `base.path`; every other pre-set flag is discarded, matching the spec's "wizard owns the tuning" decision.
- **Type consistency:** `WizardPrompter` method names (`note`/`select`/`confirm`/`input`), `WizardFlow` variants (`Run`/`PrintOnly`/`Cancelled`), and the `ProviderChoice`/`ModeChoice`/`WizardAnswers` field names are identical across Tasks 2-6.
