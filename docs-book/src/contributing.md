# Contributing

ohara is a small, focused project. Contributions are welcome — bug
reports, doc fixes, perf work, and new language support all land
through the same flow.

## Build from source

You need Rust 1.85 or newer (the toolchain is pinned in
`rust-toolchain.toml` so `rustup` picks it up automatically). From a
fresh clone:

```sh
cargo build --workspace
cargo test --workspace
```

The release binaries land at `target/release/{ohara,ohara-mcp}` after
`cargo build --release --workspace`.

## TDD style: red/green commits per task

The codebase uses a strict test-driven loop. Plans (under
`docs/superpowers/plans/`) are broken into small tasks; each task lands
as **two commits**:

1. **Red.** Write the test that pins the contract. Commit before
   implementing — the test should fail.
2. **Green.** Implement just enough for the test to pass. Commit
   the implementation separately.

This keeps the diff for any one change small and the intent
unambiguous in `git log`. The commit-per-step rule is documented in
[`memory/feedback_commit_granularity.md`](https://github.com/vss96/ohara/blob/main/memory/feedback_commit_granularity.md).

## Where specs and plans live

- `docs/superpowers/specs/` — design specs (one per release). The
  spec is written first, gets reviewed, and then a matching plan is
  drafted.
- `docs/superpowers/plans/` — implementation plans. Each plan
  enumerates tasks; each task has a clear deliverable, test, and
  exit criteria. Plans are the authoritative source for "what should
  this PR contain?".
- `docs/perf/` — perf baselines and A/B notes (e.g. the v0.6
  throughput baseline).

When in doubt, find the relevant spec and plan and follow the
commit cadence they imply.

## Test layout

- **Unit tests** live alongside the code they test, in `#[cfg(test)]
  mod` blocks. Used for pure-function contracts and trait edge cases.
- **Integration tests** live under each crate's `tests/` directory.
  Used when the test needs to exercise a real `Storage` or git repo.
- **Cross-crate end-to-end tests** live under the workspace root's
  `tests/` directory.
- **Perf harness** lives at `tests/perf/` (a workspace member, not a
  published crate). Reserved for benchmarks the main crates'
  `[dev-dependencies]` shouldn't depend on.

Always run `cargo test --workspace` before opening a PR.

## CI workflows

Three GitHub Actions workflows under `.github/workflows/`:

- `release.yml` — cargo-dist-driven release pipeline. Builds the
  per-platform binaries, generates the curl-pipe-sh installer, and
  attaches both to the GitHub Release on every tag.
- `docs.yml` — builds this mdBook site and deploys to GitHub Pages
  on every push to `main`.
- `perf.yml` — runs the `tests/perf` harness on a schedule + on
  PRs that touch retrieval-quality code paths.

`main` is branch-protected: PRs require green CI before merge, and
direct pushes are rejected.

## Pull request etiquette

- Reference the spec / plan / issue your change implements in the PR
  description.
- Keep PRs small. If a plan task takes more than a couple of
  red/green commits, split it.
- Don't bundle unrelated changes — even small refactors get their own
  PR so they're easy to revert if regression hunts pin them.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets`
  locally before pushing.
