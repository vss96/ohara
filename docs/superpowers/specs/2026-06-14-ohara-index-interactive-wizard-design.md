# ohara index interactive wizard — `ohara index -i`

Date: 2026-06-14
Status: approved (brainstorm 2026-06-14)
Related: 2026-06-09-ohara-usability-plan-design.md (adoption polish),
2026-06-13-ohara-coreml-fixed-shape-design.md (provider tradeoffs)

## Problem

`ohara index` exposes a rich tuning surface — `--embed-provider`
(auto/cpu/coreml/cuda), `--resources` (auto/conservative/aggressive),
`--commit-batch`, `--embed-batch`, `--threads`, `--workers`,
`--embed-cache` (off/semantic/diff), plus the mode flags
`--incremental` / `--force` / `--rebuild --yes` — but a new user has no
guided way to discover or choose among them. The headline question
"should I use CoreML or CPU on this machine?" requires reading the
`--help` wall and knowing the build-feature and host caveats (CoreML
needs `--features coreml` + Apple Silicon; CUDA needs `--features cuda`
+ a visible device).

We want an opt-in interactive wizard that walks the user through the
common knobs (with host-aware annotations), optionally the advanced
ones, shows the equivalent CLI command, and then runs the *existing*
index path. The wizard teaches the flags while removing the need to
memorise them.

## Goals

- `ohara index -i` (or `--interactive`) launches a sequential prompt
  wizard. Bare `ohara index` and every existing flag combination keep
  their exact current behavior.
- The happy path is short (~3 prompts: provider → intensity → mode),
  with advanced knobs behind a single opt-in confirm.
- Provider options are host- and build-aware: only offer providers
  this binary can actually run; explain any that are hidden.
- The wizard is a pure front-end: it assembles an `index::Args` and
  hands it to the unchanged `commands::index::run`. There is exactly
  one indexing code path.
- The assembly logic is unit-testable without a TTY.

## Non-goals (YAGNI)

- No full-screen / ratatui UI. Sequential prompts only.
- No saved presets or config files.
- No back-navigation / re-prompt loop (forward-only; ESC cancels).
- No changes to `query`, `explain`, or any non-`index` command.
- No pre-seeding the wizard from other flags passed alongside `-i`
  (the wizard owns the tuning when `-i` is set; see Open edge cases).

## Library choice

Use **`dialoguer`** (the `console-rs` family: `Select`, `Confirm`,
`Input`). It builds on the `console` crate already pulled in
transitively by `indicatif`, so it reuses the existing terminal
handling and adds little new dependency weight. `inquire` (richer
per-item descriptions, but pulls in `crossterm`) was the runner-up.

`dialoguer` is added to the root `Cargo.toml`
`[workspace.dependencies]` (CONTRIBUTING: workspace-only deps) and
referenced by `ohara-cli` via `dialoguer.workspace = true`.

## Flow

The wizard prompts in this order. Prompts that map to an `Option`
field treat empty / `auto` input as "leave unset" (→ `None`, so the
resource planner picks the value).

1. **Embedding provider** — `Select`.
   - Always list `Auto` (default selection, annotated with what it
     resolves to on this host, e.g. "→ CPU on this host") and `CPU`.
   - List `CoreML` only when `cfg!(feature = "coreml")` **and**
     `target_os = "macos"`; annotate "~3× faster on Apple Silicon;
     first run downloads ~130 MB and pays a one-time ~30 s compile".
   - List `CUDA` only when `cfg!(feature = "cuda")`; annotate whether
     `CUDA_VISIBLE_DEVICES` is set.
   - Emit a footnote line for any provider hidden by build features,
     e.g. "CoreML hidden: built without `--features coreml`", so an
     absent option is never mysterious.
2. **Resource intensity** — `Select`: Auto (default) / Conservative /
   Aggressive, each with a one-line description.
3. **Index mode** — `Select`:
   - `Standard` (default) — index new commits; no extra flags.
   - `Incremental` — `--incremental`; cheap no-op when HEAD is already
     indexed (the post-commit-hook fast path).
   - `Force` — `--force`; re-extract HEAD symbols (after a chunker
     change).
   - `Rebuild` — delete & rebuild the whole index from scratch.
     Selecting this triggers a second destructive `Confirm` that names
     the index DB path; only on confirm does the wizard set
     `--rebuild --yes`. Declining returns to the mode prompt's default
     (Standard).
4. **"Configure advanced knobs?"** — `Confirm`, default No. If yes,
   prompt in order:
   - `threads` — `Input`, default empty/"auto" → `None`.
   - `workers` — `Input`, default empty/"auto" → `None`.
   - `commit-batch` — `Input`, default empty/"auto" → `None`.
   - `embed-batch` — `Input`, default empty/"auto" → `None`.
   - `embed-cache` — `Select`: off (default) / semantic / diff.
   - `no-progress` — `Confirm`, default No.
   - `profile` — `Confirm`, default No.
5. **Summary & run** — print the equivalent command
   (`ohara index --embed-provider coreml --resources aggressive …`),
   then `Confirm` "Run now?" (default Yes).
   - Yes → hand the assembled `Args` to `index::run`.
   - No → the equivalent command is already printed; exit 0 without
     indexing so the user can copy/paste it.

## Architecture

All new code lives in `crates/ohara-cli/src/commands/`, each file
under 500 lines with a single responsibility.

### `index::Args` change

Add one field:

```rust
/// Launch the interactive index wizard to choose provider,
/// resource intensity, mode, and advanced knobs. Requires a TTY.
#[arg(long, short = 'i')]
pub interactive: bool,
```

### Wiring

At the top of `commands::index::run`, before any work:

```rust
let args = if args.interactive {
    crate::commands::index_wizard::run_wizard(args).await?
} else {
    args
};
```

`run_wizard` runs the (blocking) dialoguer prompts inside
`tokio::task::spawn_blocking`, then returns a fully-populated `Args`
with `interactive` cleared. Everything downstream of `run` is
untouched. The wizard runs before any indexing span is opened, so it
does not contend with the `IndicatifLayer` progress bars.

### `commands::index_wizard.rs`

Orchestrates the flow above. Talks to the terminal only through a
small trait so the logic is testable:

```rust
pub trait WizardPrompter {
    fn select(&mut self, prompt: &str, options: &[String], default: usize) -> Result<usize>;
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool>;
    fn input(&mut self, prompt: &str) -> Result<String>; // empty allowed
}
```

- `DialoguerPrompter` — the real impl wrapping `dialoguer`.
- `ScriptedPrompter` (test-only) — returns canned answers in order and
  asserts the prompt sequence, so the whole flow is exercised without
  a TTY.

Numeric fields (threads/workers/batches) are validated in
`run_wizard`, not in the prompter: it calls `input`, and on a non-empty
string that fails to parse as `usize` it re-prompts (a loop over the
trait). This keeps validation testable — a `ScriptedPrompter` can
return a bad-then-good sequence — and keeps the trait minimal (`input`
stays `String`-typed). Empty / `auto` short-circuits to `None` without
parsing.

`run_wizard` collects answers into a `WizardAnswers` struct, then calls
the pure `assemble_args`.

### Pure, testable units

- `host_capabilities() -> ProviderAvailability` — combines
  `cfg!(feature = "coreml")` / `cfg!(feature = "cuda")` with
  `resources::detect_host()` to report which providers are offerable
  and how to annotate each. Drives the provider `Select` list.
- `assemble_args(answers: WizardAnswers, base: Args) -> Args` — maps
  answers onto `Args` fields. This is where the logic worth testing
  lives: rebuild → `rebuild = true, yes = true`; force/incremental
  flags; `"auto"`/empty numeric inputs → `None`; embed-cache enum
  mapping. Carries `base.path` through unchanged.
- `args_to_command(args: &Args) -> String` — renders the equivalent
  `ohara index …` command for the summary, omitting defaults. Pinned
  by exact-output tests (mirrors the existing `index_summary_human`
  test style).

## Error handling

- **`-i` on a non-TTY** (piped stdin): detect up front via
  `console::user_attended()` / `IsTerminal` and bail with a clear
  message — "`--interactive` requires a TTY; pass explicit flags
  instead". Exit non-zero.
- **ESC / Ctrl-C mid-prompt**: dialoguer returns an interrupted error;
  `run_wizard` maps it to a clean "cancelled — nothing indexed" and
  the command exits 0.
- **Unavailable providers**: never selectable (filtered out of the
  list), so the wizard can never produce a provider the build can't
  honor — we never dead-end into the runtime "not enabled in this
  build" error.
- **Embed-cache change on an existing index**: the wizard does not
  pre-empt the existing `embed_input_mode` mismatch guard in
  `index::run`; if the chosen `--embed-cache` is incompatible with the
  stored index, the existing guard still fires with its
  rebuild-instructions message. (Optional nicety: surface a heads-up
  in the summary; not required for v1.)

## Edge cases & decisions

- **`-i` combined with other tuning flags**: the wizard owns the
  tuning surface. Any tuning flags passed alongside `-i` are ignored
  (the wizard's answers win). `path` is the one exception — it is read
  from `base.path`. Keeps the model simple; pre-seeding from flags is
  an explicit non-goal.
- **Rebuild safety**: the destructive second confirm in the wizard is
  in addition to — not a replacement for — the existing
  `assert_rebuild_safe` / `--yes` machinery in `index::run`. The
  wizard simply sets `--rebuild --yes` after its own confirm; all
  existing guardrails still run.

## Testing

Unit tests (no TTY required):

- `assemble_args` for each mode (Standard/Incremental/Force/Rebuild),
  asserting the exact flag combination — especially Rebuild →
  `rebuild && yes`, and Force/Incremental exclusivity.
- Numeric handling: `""` and `"auto"` → `None`; a valid number →
  `Some(n)`; an invalid number is re-prompted by `run_wizard`'s parse
  loop (verified with a `ScriptedPrompter` bad-then-good sequence)
  rather than silently dropped.
- `assemble_args` provider + embed-cache enum mapping.
- `host_capabilities` provider filtering per cfg flags, using
  cfg-gated assertions (mirrors the existing
  `provider_resolution_tests` pattern).
- `args_to_command` exact-output tests, including the "omit defaults"
  behavior and the rebuild/advanced cases.
- A `ScriptedPrompter` end-to-end test: feed a canned answer sequence,
  assert the resulting `Args` and that the prompt order matches.
- Non-TTY guard returns the documented error.

No new integration test harness is needed; the wizard's I/O boundary
is the `WizardPrompter` trait, and the rest is pure functions.

## Docs

- Update `docs-book/` where `ohara index` flags are documented to
  mention `-i` / `--interactive`.
- The `-i` flag's clap doc-comment is the primary `--help` surface.
