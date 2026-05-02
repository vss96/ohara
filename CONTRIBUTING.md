# Contributing to Ohara

This document is the **binding standard** for all contributions — human or AI agent — to this repository. It uses [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) keywords (**MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY**) to make rules unambiguous.

If you are an agent: treat every **MUST** as a hard precondition. If a rule conflicts with the user's explicit instruction, the user wins — but call out the conflict in your reply.

---

## 1. Workspace Layout

The workspace is organised as a set of focused crates under `crates/`:

| Crate | Role | Kind |
|---|---|---|
| `ohara-core` | Domain types, traits, orchestration | Library |
| `ohara-storage` | SQLite + sqlite-vec persistence, migrations | Library |
| `ohara-embed` | Embedding model providers (fastembed) | Library |
| `ohara-git` | Git interaction (libgit2) | Library |
| `ohara-parse` | Tree-sitter parsing | Library |
| `ohara-cli` | End-user CLI binary | Binary |
| `ohara-mcp` | MCP server binary | Binary |
| `tests/perf` | Performance benchmarks | Test crate |

### Dependency direction

- Binaries (`ohara-cli`, `ohara-mcp`) **MAY** depend on any library crate.
- Libraries **MUST NOT** depend on binaries.
- `ohara-core` is the dependency root: it **SHOULD** define traits that other library crates implement (Dependency Inversion). `ohara-core` **MUST NOT** depend on `ohara-storage`, `ohara-embed`, `ohara-git`, or `ohara-parse` for concrete behaviour — only on the traits it owns.
- Cross-library dependencies between leaves (e.g. `ohara-embed` depending on `ohara-git`) **MUST** be justified in the PR description.

---

## 2. Design Principles

These are the rules that shape *how* code is written, not just how it's formatted. Reviewers and agents are expected to push back on violations.

### 2.1 Expose behaviour through traits

- Public capabilities of a crate **MUST** be expressed as traits. Concrete types are implementation detail.
- Higher crates **MUST** depend on traits owned by `ohara-core` (or the consuming crate), not on concrete types from leaf crates. Example: `ohara-core` defines `trait EmbeddingProvider`; `ohara-embed` provides an implementation; `ohara-cli` accepts `impl EmbeddingProvider` or `Arc<dyn EmbeddingProvider>`.
- Trait surface **SHOULD** be small and focused (Interface Segregation). Prefer two narrow traits over one wide one.

### 2.2 SOLID

- **SRP** — One type, one reason to change. If a struct has two unrelated reasons to change, split it. The 500-line file ceiling (§7) is a backstop for this.
- **OCP** — Add behaviour by adding new types/impls, not by editing existing match arms unless the variant genuinely belongs in the same domain.
- **LSP** — Trait impls **MUST** honour the trait's documented contract. Surprising panics, hidden I/O, or weaker error guarantees in an impl are violations.
- **ISP** — Prefer many small traits. A consumer **SHOULD NOT** be forced to depend on methods it does not call.
- **DIP** — See §2.1.

### 2.3 Object Calisthenics (adopted subset)

The following rules from Object Calisthenics apply. Rules not listed here are **not** part of this standard.

- **No `else`.** Use early returns / guard clauses. `match` is not an `else` for this purpose and is encouraged.
- **Wrap primitives that carry meaning in newtypes.** A path-to-a-repo, an embedding dimension, a commit SHA, a file content hash — these **MUST NOT** be passed as bare `String`/`PathBuf`/`usize`/`u64` across module boundaries. Define `RepoPath`, `EmbeddingDim`, `CommitSha`, `ContentHash` etc.
- **Keep entities small.** Files **MUST** stay under 500 lines. Functions **SHOULD** stay under 50 lines. Types **SHOULD** have a single, namable responsibility.
- **Expose behaviour, not state.** Public getters/setters that simply mirror fields are a smell. Prefer methods named for the operation the caller wants to perform.
- **No abbreviations in names.** `idx`, `cfg`, `req`, `res`, `mgr`, `svc` are forbidden. Use `index`, `config`, `request`, `response`, `manager`, `service`. Established Rust idioms are exempt: `i`/`j`/`k` for loop indices, `it` for iterators, `e` for the error in `match` arms, `f` for a formatter, `Self`/`self`.

### 2.4 Avoid long chains of `if`

Long `if` / `else if` ladders are a refactor signal.

- For closed sets of variants: use `match` with exhaustive arms.
- For dispatch on type or strategy: use a trait + impls, or `enum` + `match`.
- For preconditions: use early-return guard clauses at the top of the function.
- Cyclomatic complexity **SHOULD** stay low; clippy's `cognitive_complexity` lint is enabled (§5).

### 2.5 Clean code essentials

- Names reveal intent. If you need a comment to explain a name, rename it.
- Functions do one thing at one level of abstraction.
- Dead code **MUST** be deleted, not commented out — git history is the archive.
- Comments explain *why*, not *what*. The code already says what.

---

## 3. Error Handling

- Library crates (`ohara-core`, `ohara-storage`, `ohara-embed`, `ohara-git`, `ohara-parse`) **MUST** use `thiserror` for typed errors. Each library **SHOULD** expose a single top-level `Error` enum (e.g. `ohara_storage::Error`).
- Binary crates (`ohara-cli`, `ohara-mcp`) and tests **MAY** use `anyhow` at the boundary where errors become user-facing.
- Library code **MUST NOT** depend on `anyhow`.
- `unwrap()`, `expect()`, and `panic!()` are **forbidden in non-test code**. The only exceptions:
  - `expect("invariant: <reason>")` where the message documents an invariant that genuinely cannot fail. The justification **MUST** be in the message itself.
  - `main()` in binaries **MAY** propagate via `Result<(), anyhow::Error>` and rely on the framework's panic behaviour for unrecoverable startup errors.
- When propagating with `?` across a crate boundary, **SHOULD** add context (`thiserror`'s `#[from]` is fine; for `anyhow` use `.context("...")`).
- Errors **MUST** be logged at most once, at the binary boundary. Library code **MUST NOT** log errors it returns.

---

## 4. Async, Concurrency, Logging

- Use `tokio` for I/O-bound work.
- Use `rayon` for CPU-bound work (parsing, embedding inference, batch transforms).
- **MUST NOT** call `block_on` or any other blocking runtime API from inside an async function. Move CPU work to `tokio::task::spawn_blocking` or `rayon`, then `await` the join handle.
- Spawned tasks **MUST** be either awaited, joined into a `JoinSet`, or have their cancellation behaviour documented. Fire-and-forget tasks are a code smell.
- All logging **MUST** go through `tracing`. `println!` and `eprintln!` are reserved for **user-facing CLI output in `ohara-cli`** (e.g. command results). They **MUST NOT** appear in any other crate.
- Spans **SHOULD** be added at meaningful operation boundaries (one indexing run, one query, one MCP request) and **SHOULD** carry structured fields rather than formatted strings.

---

## 5. Lints, Formatting, Toolchain

- Toolchain is pinned in `rust-toolchain.toml` (stable). **MUST NOT** introduce nightly-only features.
- `cargo fmt --all` **MUST** pass with no diff.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` **MUST** pass.
- The workspace enables (or will enable, as part of adopting this document):
  - `deny(warnings)` in CI
  - `warn(clippy::pedantic)`, `warn(clippy::nursery)`
  - `warn(clippy::cognitive_complexity)`
  - `forbid(unsafe_code)` workspace-wide
- `unsafe` is **forbidden**. If a genuine need arises, it **MUST** be in its own module behind a safe API, with a `// SAFETY:` block on every `unsafe` block, and called out in the PR.

---

## 6. Visibility & Module Hygiene

- Default visibility is `pub(crate)`. Items are made `pub` **only** when they are part of the crate's intentional external API.
- Re-exports at `lib.rs` define the crate's public surface. If it is not re-exported (or `pub` in a `pub mod`), it is not API.
- Public items in library crates **MUST** carry `///` doc comments. Every `lib.rs` **MUST** carry a crate-level `//!` doc comment summarising the crate's role and key traits.

---

## 7. File and Function Size

- Files **MUST** stay under 500 lines. If a file approaches the limit, split by responsibility, not by line count.
- Functions **SHOULD** stay under 50 lines. Long functions are a refactor signal, not a hard error.
- Types with more than ~7 fields are a refactor signal — consider grouping related fields into a sub-struct.

---

## 8. Testing

- Unit tests live inline in the same file as the code under test, in a `#[cfg(test)] mod tests` block.
- Integration tests live in `crates/<crate>/tests/`.
- Performance benchmarks live in `tests/perf/`.
- Every bug fix **MUST** ship with a regression test that fails before the fix and passes after.
- Every new public function or trait method **SHOULD** have at least one test exercising its happy path and at least one exercising a documented failure mode.
- Async tests use `#[tokio::test]`; CPU-heavy tests **MAY** use plain `#[test]`.
- Fixtures live in `fixtures/` and **MUST NOT** be modified casually — they are part of the test contract.

---

## 9. SQL & Migrations

- All SQL **MUST** live in `ohara-storage`. No other crate may construct SQL strings.
- Migrations live in `crates/ohara-storage/migrations/` and follow `refinery`'s `V{N}__snake_case_description.sql` naming.
- Migrations are **append-only**. Once a migration has been merged to the main branch, it **MUST NOT** be edited. Fix mistakes by adding a new migration.
- Schema-touching changes **MUST** include a migration in the same PR.

---

## 10. Dependencies

- All third-party dependencies **MUST** be declared in the root `Cargo.toml` under `[workspace.dependencies]`. Crate `Cargo.toml`s reference them via `dep.workspace = true`.
- Adding a new dependency requires a one-line justification in the PR description (what it does, why we need it, what we considered instead).
- Default features **SHOULD** be disabled (`default-features = false`) when only a subset is used. The fastembed, git2, and rmcp deps already follow this pattern — preserve it.
- Version specifiers **SHOULD** be conservative (`"1"`, not `"1.2.3"`) unless a specific version is pinned for a documented reason (see the `tracing-indicatif` pin in `Cargo.toml` for the format of such a comment).

---

## 11. Commits & Pull Requests

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `refactor:`, `chore:`, `docs:`, `test:`, `perf:`, optionally scoped by crate (`feat(storage): ...`, `fix(parse): ...`).
- One logical change per commit. Use `git rebase -i` to clean up before merging.
- PR descriptions **MUST** describe *what* changed and *why*, list any new dependencies, and call out any rule in this document that the PR intentionally bends.

---

## 12. Documentation Files

- **MUST NOT** create new top-level documentation files (`*.md`) unless the user explicitly asks for them by name or purpose.
- Per-crate `README.md` files are allowed when a crate has non-obvious operational concerns.
- Architecture or design notes belong in `docs/` and **MUST** be requested explicitly.

---

## 13. Agent Checklist (must pass before reporting "done")

An agent **MUST NOT** declare a task complete until all of the following hold:

1. `cargo fmt --all` produces no diff.
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings` is clean.
3. `cargo test --workspace` is green.
4. No new `unwrap()`, `expect()` (without justification), `panic!()`, `println!`, or `eprintln!` introduced outside their allowed locations.
5. No new dependency added outside `[workspace.dependencies]`.
6. No existing migration file edited.
7. No new `*.md` file created unless the user asked for one.
8. Any new public item in a library crate carries a `///` doc comment.
9. Any rule in this document the change bends is explicitly called out in the agent's reply.

If unsure about an API boundary, a trait split, or whether a refactor is in scope: **stop and ask** rather than guess.
