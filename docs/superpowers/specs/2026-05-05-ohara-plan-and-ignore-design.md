# ohara — `ohara plan` + `.oharaignore` (smart indexing, part 1 of 3)

**Status:** RFC — ready to plan.

**Supersedes:** `docs/superpowers/specs/2026-05-04-ohara-max-commits-design.md`
(same problem class — making giant repos viable — but with a blunter answer).

**Companion specs (separately scoped, brainstormed later):**
- Spec B — chunk-level content dedup (embed reuse via content hash).
- Spec D — parallel commit pipeline (worker pool + in-order watermark
  serializer). Sequenced *after* A and B so the stages it parallelises
  are stable.

## Goal

Make `ohara index` selective. Walk the full history, but skip parse +
embed work for paths and commits that don't carry signal. The user opts
into the skip-set by running `ohara plan`, which surveys the repo,
prints a directory hotmap, and writes a `.oharaignore` the user reviews
before indexing.

## Why this matters

Today `ohara index <repo>` parses and embeds every changed file in
every reachable commit. That model holds up to mid-six-figure histories
but breaks on giant repos:

```
$ ohara status /Users/vss/Documents/linux
last_indexed_commit: <none>
commits_behind_head: 1,444,500
```

The naive answer (the superseded RFC) was `--max-commits N`: cap the
walk to the most recent N. Blunt — discards 99% of the history,
including the parts the user actually wants context on.

The smarter answer: most of those 1.44M commits are signal-poor by
*location*. Roughly half of Linux's commit volume lands in `drivers/`,
much of `arch/` is non-x86 churn the average user doesn't care about,
generated headers and vendored toolchains compound the noise. A
`.oharaignore` that drops these preserves the *breadth* of history
(`Documentation/`, `kernel/`, `fs/`, `mm/`, `net/`, `block/`, …) while
cutting ~60-70% of the cold-pass cost.

This spec is necessary but not sufficient for "ohara works on Linux."
Specs B (chunk-dedup) and D (parallel pipeline) carry the rest of the
weight on the embed and CPU axes. The three are independent enough that
A and B can be implemented in parallel by separate agents; D depends on
both.

## Shape

### `ohara plan [path]`

New CLI subcommand. Pre-flight planner that runs a diff-only libgit2
walk, scores directories, prints suggestions, and (with confirmation)
writes `.oharaignore`.

```
ohara plan [PATH]                  # PATH defaults to "."
  --yes                            # write .oharaignore without prompting
  --no-write                       # print suggestions only, never write
  --keep-existing                  # default; preserves user lines outside
                                   # the auto-generated section
  --replace                        # overwrite the entire .oharaignore
```

Pipeline (all stages stream — no full-materialise of the diff list):

1. **Walk.** `Repository::revwalk()` over HEAD-reachable commits. For
   each commit, ask libgit2 for the changed-paths list only (no text,
   no rename detection). ~1 ms per commit on M-series; ~25-40 min for
   Linux's 1.44M commits.
2. **Aggregate.** For each changed path, increment a counter for every
   path prefix (`drivers/staging/foo.c` bumps `drivers/`,
   `drivers/staging/`, `drivers/staging/foo.c`). Output:
   `BTreeMap<RepoPath, u64>` of commit-count per directory. Keeping
   the walk path-only (no diff text, no diff stats) is what makes the
   pre-flight feasible on giant repos — adding line-count would
   multiply the walk cost by ~10×.
3. **Score.** Top-level directories are ranked by commit share. A
   directory is a "high-share" suggestion candidate when its commit
   share exceeds a threshold (default 5%). The mechanical-vs-human
   distinction is carried by the built-in pattern list + the
   documentation allowlist — we don't try to infer it from diff shape
   in the path-only walk.
4. **Suggest.** Propose ignoring:
   - Built-in defaults (lockfiles, `node_modules/`, `target/`,
     `vendor/`, `dist/`, `.git/`, `.next/`, `build/`, `__pycache__/`,
     `*.min.js`, `*.lock`, etc. — full list in `ohara-core`).
   - High-share top-level directories *not* in a small documentation
     allowlist (`Documentation/`, `docs/`, `README*`, `LICENSE*`).
5. **Render.** Print a hotmap table (top 20 directories), per-directory
   suggestions with rationale, and estimated work-saved %. Prompt for
   confirmation unless `--yes` or `--no-write`.
6. **Write.** Emit `.oharaignore` at repo root, with the auto-generated
   section between delimiters.

Sample output:

```
$ ohara plan ~/Documents/linux
walking 1,444,500 commits... done in 41m 12s
──────────────────────────────────────────────────────────────
top directories by commit share:
  drivers/             721,003  (49.9%)  → IGNORE (high share, not in docs allowlist)
  arch/                154,892  (10.7%)  → IGNORE non-x86 subdirs
  tools/                82,401   (5.7%)  → IGNORE (high share, not in docs allowlist)
  Documentation/        43,887   (3.0%)  → keep (docs allowlist)
  fs/                   41,228   (2.9%)  → keep
  net/                  38,910   (2.7%)  → keep
  ...

proposed auto-generated section:
  drivers/
  arch/alpha/
  arch/arm/
  arch/arm64/
  arch/m68k/
  arch/mips/
  ...

estimated work saved: 64.3% of commits drop, 71.1% of changed paths
note: rebuild with --features coreml (Apple) or --features cuda (NVIDIA)
      for ~3-5× embed speedup on repos this size.

write .oharaignore? [y/N]
```

### `.oharaignore` format

Gitignore-syntax patterns, one per line, at the repo root. Matched at
index time using the `ignore` crate (already a workspace dep
transitively through `walkdir`/`globset` — promote to direct workspace
dep).

Three sources are merged into one `LayeredIgnore` filter, in priority
order (lower number wins):

1. **Built-in defaults.** Compiled into `ohara-core`. Always applied;
   overridable from `.oharaignore` with `!pattern` negations.
2. **`.gitattributes`.** Paths flagged `linguist-generated=true` or
   `linguist-vendored=true` are auto-ignored.
3. **`.oharaignore` at repo root.** User-controlled, `ohara plan`-edited.

The auto-generated section is delimited:

```
# === ohara plan v0.7.7 — auto-generated 2026-05-05T14:23:11 ===
drivers/
arch/alpha/
arch/arm/
# === end auto-generated ===

# user-added below this line is preserved by `ohara plan --keep-existing`
!Documentation/
my-team-specific-skip/
```

`ohara plan --keep-existing` (default) replaces only the section between
the markers. If the markers are missing on a non-empty file, the command
*fails open* — refuses to merge, prints what it would do, asks the user
to either delete the file and re-run, or pass `--replace` to overwrite.

### Indexer change

A new module `crates/ohara-core/src/ignore.rs`:

```rust
pub trait IgnoreFilter: Send + Sync {
    fn is_ignored(&self, path: &RepoPath) -> bool;
}

pub struct LayeredIgnore { /* built-ins + gitattributes + user file */ }

impl LayeredIgnore {
    pub fn load(repo_root: &Path) -> Result<Self>;
}

impl IgnoreFilter for LayeredIgnore {
    fn is_ignored(&self, path: &RepoPath) -> bool { /* ... */ }
}
```

Integration into plan-19's pipeline at the **chunker stage**: the
chunker today receives a list of `(path, hunk_text)` pairs from the
diff stage. It gains a filter pass:

```rust
let mut paths_total = 0;
let mut paths_kept = 0;
for (path, hunk) in diff_entries {
    paths_total += 1;
    if filter.is_ignored(path) { continue; }
    paths_kept += 1;
    // ... existing chunking logic
}
```

After the loop:

- `paths_kept > 0` — index the surviving hunks normally.
- `paths_kept == 0 && paths_total > 0` — the commit is **100% ignored**.
  Skip it: no `commit::put`, no `vec_hunk` rows, no `fts_*` rows. The
  watermark **still advances** past this commit (it's "processed" from
  the resume model's perspective; we just chose to write nothing).
- `paths_total == 0` — empty commit (merge with no diff, etc.).
  Existing behaviour unchanged.

### Storage change

None. The skip decision is recomputed from `.oharaignore` on every
indexer run. There is no persisted "this commit was skipped because"
record. Auditability of skips, if we ever want it, is a separate spec.

### CLI / status surface

`ohara status` learns one new line when `.oharaignore` is present:

```
ignore_rules: 14 patterns (3 built-in + 0 gitattributes + 11 user)
```

When the binary is built without `--features coreml` and without
`--features cuda`, both `ohara plan` and `ohara index --rebuild` print
a one-line note recommending the rebuild for repos over a size threshold
(e.g., `commits_behind_head > 100_000`). Free win — no new code paths,
just surfaces an existing capability.

## Constraints

- **`.oharaignore` is read at index time, not stored.** A user edits
  the file → next run honors it. No DB migration on edits.
- **Skipped commits still advance `last_indexed_commit`.** Resume after
  abort works exactly as today.
- **The auto-generated marker is required.** Without it, `ohara plan
  --keep-existing` fails open (refuses to silently overwrite user
  content). User can delete + re-run or pass `--replace`.
- **No silent skips.** `ohara plan` always prompts unless `--yes` /
  `--no-write` make consent explicit at the CLI level. Agents using
  `--yes` are explicit about their intent.
- **`.oharaignore` lives at repo root, not `.ohara/`.** It's checked
  into the repo and shared by all team members; it's a team artifact,
  not a per-user setting.
- **No `--max-commits` flag in this spec.** That's a separate axis (time
  budget vs noise filtering); the original RFC for it is superseded.
  Revisit only if A+B+D together don't make Linux viable.

## Non-goals

- **Bot-author / commit-message detection** (dependabot, renovatebot,
  conventional-commit prefixes). Path-level filtering already catches
  these — their commits are almost always lockfile-only, and lockfiles
  are in the built-in defaults. The rare mixed-content bot PR keeps its
  real signal.
- **Diff-shape heuristics** (huge-uniform-hunk detection, formatter-pass
  detection). High false-positive risk; defer until evidence shows we
  need them.
- **Auto-suggest on `ohara index`.** `ohara plan` is its own subcommand;
  `ohara index` doesn't run a pre-flight unprompted.
- **Per-branch ignore variation.** `.oharaignore` is a single repo-root
  file. Multi-worktree setups inherit one ignore set (matches how
  `.gitignore` works).
- **Automatic re-planning on `--incremental`.** User runs `ohara plan`
  again if their interests change; we don't re-survey on every update.
- **Storing skip-audit metadata.** No "this commit was dropped because
  X" record. The decision is reproducible from `.oharaignore` + the
  built-ins.

## Success criteria

- `ohara plan ~/Documents/linux` finishes the diff-only walk and writes
  `.oharaignore` in under 60 minutes on an M-series box.
- `ohara plan` is idempotent: running twice on a repo at the same HEAD
  produces byte-identical `.oharaignore` modulo timestamps.
- `ohara plan --keep-existing` (default) preserves user lines outside
  the auto-generated section across re-runs.
- A unit test on `LayeredIgnore` covers: built-in default match, user
  pattern match, gitattributes match, `!negate` override of a built-in,
  and precedence ordering across all three sources.
- An integration test in `crates/ohara-cli/tests/`: a fixture with mixed
  paths gets a `.oharaignore` containing `vendor/`. `ohara index`
  on the fixture skips vendor hunks, indexes the rest, and the
  watermark reaches HEAD.
- Integration test: a commit whose entire diff is in `vendor/` produces
  zero rows in `commit`, `vec_hunk`, `fts_hunk` after indexing — and
  `last_indexed_commit` advances past it.
- Regression test: running `ohara index --rebuild --yes` *without* a
  `.oharaignore` produces the same row counts as today (no behaviour
  change for users who don't opt in).

## Out of scope (deferred to companion specs)

- **Spec B — Chunk-level content dedup.** Hash chunk content before
  embed; reuse stored vectors when `(content_hash, embed_model)` is
  already present in `vec_hunk`. ~2-5× embed-time reduction on giant
  repos. Schema migration on `vec_hunk`. Independent of this spec —
  different pipeline stage, different storage surface.
- **Spec D — Parallel commit pipeline.** Worker pool around parse +
  embed; in-order serializer that buffers out-of-order finished commits
  and writes them in topo order to keep watermark/resume semantics.
  ~3-6× wall-time on multi-core boxes. Sequenced after A+B so the
  stages it parallelises are stable.
- **Backfill of older history past a `--max-commits`-style window.** If
  A+B+D ship and Linux is still painful, revisit a time-budget flag as
  a backstop.

## Related

- `crates/ohara-cli/src/commands/plan.rs` — new file (subcommand entry +
  hotmap renderer + writer; ≤ 500 lines).
- `crates/ohara-cli/src/main.rs` — register the `Plan` subcommand.
- `crates/ohara-cli/src/commands/status.rs` — render `ignore_rules` line.
- `crates/ohara-core/src/ignore.rs` — new module: `IgnoreFilter` trait,
  `LayeredIgnore` impl, built-in defaults list.
- `crates/ohara-core/src/indexer.rs` — wire `IgnoreFilter` into the
  chunker stage of plan-19's pipeline; implement the 100%-paths-ignored
  commit-skip rule.
- `crates/ohara-git/src/walker.rs` — gains a paths-only diff helper used
  by `ohara plan`'s pre-flight walk (no text, no rename detection).
- `Cargo.toml` (workspace) — promote the `ignore` crate to a direct
  workspace dep if it isn't already.
- `docs-book/src/architecture/indexing.md` — section on `.oharaignore`
  semantics, `ohara plan`, and the three-source precedence model.
- `README.md` — add `ohara plan` to the quickstart; mention
  `--features coreml` / `--features cuda` for repos this size.
- Prior art (superseded): `docs/superpowers/specs/2026-05-04-ohara-max-commits-design.md`.
