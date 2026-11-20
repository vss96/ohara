# ohara harness — unattended one-shot coding agent runs, design

**Date:** 2026-11-20
**Status:** Draft, awaiting user approval
**Related:** 2026-04-30-ohara-context-engine-design.md (v1),
2026-06-09-ohara-daemon-consolidation-design.md (plan-29, shared daemon),
2026-06-13-ohara-single-writer-design.md (plan-32, write pool),
2026-05-02-ohara-v0.6.3-resume-skip-rfc.md (watermark and resume),
plan-10 (retrieval eval harness), plan-14 (perf harness)

## 1. Problem

ohara answers "how was X done before?" for an agent a human is driving.
The next step is to take the human out of the loop for well-scoped tasks:
hand over an issue, a prompt or a link and get back a pull request that
already passes lint and tests.

Stripe's Minions is the reference. Its lessons, as far as they transfer to
one developer's laptop:

- The workflow is **code, not a prompt**. Deterministic nodes (lint, push)
  and agent nodes (implement, fix) alternate in a "blueprint"; the model
  never decides whether lint runs.
- Context is **pre-hydrated deterministically** from the task's links
  before the agent starts.
- Feedback loops are **short and capped**: fast lints, at most two fix
  rounds, then hand back to a human.
- The environment is **isolated and pre-warmed**, so a run starts in
  seconds and cannot damage the developer's checkout.
- Each agent gets a **curated subset** of tools.

ohara already owns the piece the others had to build or skip: a local,
warm, per-repo lineage index (`find_pattern`, `explain_change`) behind a
shared daemon. The harness is the workflow, isolation and run bookkeeping
around it.

## 2. Goals & non-goals

### In scope

- `ohara run <task>`: one task in, one draft PR (or a clearly failed run)
  out, with no prompts in between.
- Local-first: runs on the developer's machine against a local clone,
  using the developer's existing `git`, `gh` and agent-CLI logins.
- Provider-agnostic agent node behind a trait.
- Resume after crash or reboot from a SQLite run log.
- A replay-based eval harness so changes to prompts, hydration and
  blueprints are judged by numbers.

### Explicitly out of scope

- A hosted or multi-user service, a web UI, Slack/issue-comment triggers.
- Our own LLM client and agent loop (see §4; revisit only if evals show
  the CLI runtimes are the bottleneck).
- A general DAG/workflow engine or a blueprint DSL (see §6).
- Auto-merge. Every PR is a draft that a human reviews.
- Multi-repo tasks; one run targets one repo.
- Cloud devboxes (Modal, remote VMs) and browser/screenshot verification.
- Learned memory on the critical path. Memory is the last phase and a run
  must behave identically with it switched off.
- Windows.

### Success criteria

- On this repo, `ohara run --issue <n>` for a `ready-for-agent` issue
  produces a draft PR whose local verify (fmt, clippy, tests) is green, or
  exits with state `needs_human` and a readable failure summary.
- Time from `ohara run` to the agent's first token under 15 s with a warm
  slot (§5) and an already-indexed base. Indexing is never on a run's
  critical path (§3).
- A run killed with `kill -9` resumes with `ohara runs resume <id>`
  without redoing completed steps.
- `ohara eval` reports a pass rate over a replay set, so two harness
  versions can be compared.

## 3. Architecture overview

```
 ohara run <task>                       ohara runs / logs / resume
        │                                         │ (read)
        ▼                                         ▼
 ┌──────────────────────────────┐      ┌─────────────────────────┐
 │ run supervisor (one process  │─────▶│ ~/.ohara/<repo-id>/     │
 │ per run, ohara-harness)      │ write│   harness.sqlite        │
 │                              │      │   runs/<run-id>/*.jsonl │
 │  Blueprint                   │      └─────────────────────────┘
 │   prepare ─ hydrate ─ agent  │
 │   ─ settle ─ verify ⟲ fix    │ IPC  ┌─────────────────────────┐
 │   (≤2) ─ guard ─ push ─ pr   │─────▶│ ohara serve daemon      │
 └───────┬───────────┬──────────┘      │  queries (read pool)    │
         │           │                 │  indexing (the single   │
         ▼           ▼                 │  writer of index.sqlite)│
 ┌────────────┐ ┌──────────────────┐   └────────────▲────────────┘
 │ Sandbox    │ │ AgentRuntime     │                │ IPC (read only)
 │ slot:      │ │ claude / codex / │   MCP stdio    │
 │ worktree   │ │ opencode headless│───────────────▶ ohara-mcp
 │ inside srt │ └──────────────────┘   (toolshed, subset per step)
 └────────────┘
       all slots are worktrees of one clone ──▶ one RepoId, one index.sqlite
```

### Key invariants

- **The blueprint owns control flow.** Agents edit code; only
  deterministic nodes run verify, push and open PRs.
- **One store per repository, shared by every worktree** (next
  subsection). Agents and supervisors only read it; the daemon is its
  only writer.
- **Indexing is never on a run's critical path.** Indexing is slow and
  queries are fast, so base history is indexed ahead of time and a run's
  own commits are indexed in the background.
- **The daemon does not host runs** (§9). It gains an indexing write
  path, nothing else.
- **The worktree and its branch are the run's working state;
  `harness.sqlite` is the run's bookkeeping.** Nothing else needs to
  survive a crash.
- **Harness configuration is read from the base commit**, never from the
  worktree the agent has edited.
- **Harness state never lives in `index.sqlite`.** `ohara index --rebuild`
  deletes that file (`crates/ohara-cli/src/commands/index/rebuild.rs`);
  run history and memories must survive it.

### One store for all worktrees (decided; phase-1 prerequisite)

**Decided (user, 2026-11-20): one ohara store serves the main clone and
every linked worktree.** Today a worktree has its own canonical path,
hence its own `RepoId` and an empty index. The harness would otherwise
have to re-index per slot or point tools at another path. This work
lands before the harness MVP (§13, phase 1a) and is useful on its own to
anyone who uses worktrees.

**RepoId.** Resolve the path through git2 `Repository::commondir()`
before hashing, so a linked worktree yields the main clone's `RepoId`
and `index.sqlite`. Separate clones keep distinct ids. For a plain clone
the hashed path must stay byte-identical to today's (the working
directory, not `.git`), so existing indexes keep their id. The
resolution goes in one helper used by every caller of
`RepoId::from_parts` (`crates/ohara-core/src/types.rs:11`):
`crates/ohara-engine/src/engine.rs:167` and `:286`, plus the CLI's
`index` and `status` commands.

**Multi-tip indexing.** Today there is one watermark,
`repo.last_indexed_commit`, and the walker does `push_head()` then
`hide(watermark)` (`crates/ohara-git/src/walker.rs:50,84`). Replace it
with a tip per worktree:

```sql
-- V7__indexed_tip.sql (append-only; backfills from the old watermark)
CREATE TABLE indexed_tip (
  repo_id      TEXT NOT NULL,
  worktree_key TEXT NOT NULL,       -- '' for the main worktree, else the linked worktree's name
  sha          TEXT NOT NULL,
  updated_at   INTEGER NOT NULL,
  PRIMARY KEY (repo_id, worktree_key)
);
INSERT INTO indexed_tip (repo_id, worktree_key, sha, updated_at)
  SELECT id, '', last_indexed_commit, strftime('%s','now')
  FROM repo WHERE last_indexed_commit IS NOT NULL;
```

Indexing worktree W walks from W's HEAD and hides **every** indexed tip,
so only commits no worktree has indexed are embedded. A tip whose object
no longer exists is skipped, not an error. `commit_record` and `hunk`
are keyed by sha and need no change. `repo.last_indexed_commit` stays in
the schema, unread after V7. `Storage::set_last_indexed_commit` and
`IndexStatus` become per-worktree; "commits behind" is computed against
the querying worktree's HEAD.

**Per-worktree visibility.** A query from worktree W returns only
commits that are W's HEAD or its ancestors, so unmerged or failed run
branches never appear as another run's prior art.

- A small trait in `ohara-core`, implemented in `ohara-git` with
  `graph_descendant_of`:

  ```rust
  // ohara-core::visibility
  pub trait CommitVisibility: Send + Sync {
      /// True when `commit` is `head` or an ancestor of it.
      fn is_visible(&self, head: &CommitSha, commit: &CommitSha) -> Result<bool>;
  }
  ```
- `Retriever` applies it as a post-filter after fusion and before the
  cross-encoder, over-fetching from the lanes (default 3× the rerank
  pool, bounded) so filtered hits do not starve the result.
- An LRU keyed by `(head, sha)` sits beside `BlameCache` in
  `ohara-engine::cache`. Ancestry between two fixed commits never
  changes, so entries never need invalidating.
- Fast path: when every indexed tip is visible from the querying HEAD
  (the normal case with no run branches), the filter is skipped.
- Cost: a few milliseconds per query, less once the LRU is warm.
- This touches ranking, so the change runs the plan-10 eval and quotes
  its summary line (CONTRIBUTING §14).

**Engine handle split.** `RepoHandle` is cached per `RepoId` but holds
one `repo_path` and one `Blamer`. With a shared id, storage and
retriever stay per `RepoId`, while the `Blamer`, the resolved HEAD and
`BlameCache` / `MetaCache` entries become per worktree path. The
existing `Request.repo_path` already tells the daemon which worktree is
asking.

**Single writer.** Every index write goes through the daemon's
`write_pool` (plan-32). New IPC methods on `RequestMethod`:
`EnsureIndexed { sha }` (blocking; answers at once when `sha` is
reachable from an indexed tip), `IndexTip` (returns immediately; the
daemon queues the worktree's HEAD and coalesces repeats), and
`PruneUnreachable`. Agents and `ohara-mcp` only read; WAL makes
concurrent readers safe. A foreground `ohara index` routes to the daemon
when one is running; `--rebuild` stays in-process and refuses to run
while any run is `running`.

**Keeping indexing off the critical path.**

- Base history is indexed once, ahead of time, on the main clone (the
  post-commit hook and `ohara index` already do this).
- `prepare` sends `EnsureIndexed { base_sha }`. Normally that is a
  reachability check. If the base is not covered, the daemon runs one
  incremental index before the run starts, and the run log records that
  it happened and how long it took. A run never indexes from scratch;
  an unindexed repo fails `prepare` with a pointer to `ohara index`.
- After each agent or fix commit the supervisor sends `IndexTip` and
  moves on. Hydration and fix rounds use whatever is indexed at that
  moment.
- `explain_change` on a commit that is not indexed yet falls back to
  plain git metadata (author, date, message) through the existing
  `CommitSource` seam, flagged in the response as not yet indexed.
- Parallel runs do not multiply embedding cost: slots share one index,
  the base is embedded once, and each run adds only its own few commits.

**Unchanged, accepted.** The head-snapshot symbol lane keeps describing
the main clone's HEAD. For a run this is slightly stale (it misses
symbols the run added and may name ones it removed). Accepted; the other
four lanes and the agent's own search cover it.

**Cleanup.** `ohara runs clean` sends `PruneUnreachable`: commits (and
their hunks, vectors and FTS rows) unreachable from any ref are deleted,
along with `indexed_tip` rows of removed worktrees.

## 4. Agent runtime

**Decided (user, 2026-11-20): the agent node drives existing agent CLIs
headless — Claude Code, Codex, OpenCode — behind a provider-agnostic
`AgentRuntime` trait.** Phase 1 ships `ClaudeCodeRuntime`; a second
adapter (`CodexRuntime`, then `OpenCodeRuntime`) lands in phase 3 to prove
the trait is honest.

```rust
// ohara-harness::agent
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn run(&self, request: AgentRequest) -> Result<AgentOutcome>;
}
```

- `AgentRequest`: working directory, prompt, `ToolPolicy` (allowed
  built-in tools, MCP server config, shell allowlist), `ModelName`,
  turn and wall-clock limits, transcript path, optional `SessionRef` to
  continue.
- `AgentOutcome`: `Completed | TurnLimit | TimedOut | Failed`, final
  message, `SessionRef` if the runtime has one, token and cost figures
  when the runtime reports them.
- An adapter is a subprocess wrapper: build argv, write an MCP config
  file, stream the CLI's JSON event output to the transcript file, map the
  exit to `AgentOutcome`. Each adapter has a fixture-recorded transcript
  test.
- **Claude Code:** `claude -p --output-format stream-json --verbose`
  (stream-json requires `--verbose` in print mode),
  `--strict-mcp-config --mcp-config <file>` so the user's global MCP
  servers do not leak into a run, an explicit `--permission-mode`, and
  `--allowedTools` / `--disallowedTools` rendered from `ToolPolicy`.
- **Codex:** `codex exec --json --sandbox <mode>`. Codex has no
  per-command allowlist, so for Codex the shell part of `ToolPolicy` is
  enforced by the `Sandbox` layer (§5), not by the CLI.
- These flags are **to be verified against the installed CLI versions**
  in the plan; `ohara harness doctor` checks them at runtime.
- Authentication is whatever the CLI already has. The harness never reads
  or stores an LLM key.

"Provider-agnostic" is therefore delivered at the runtime level (Claude
Code, Codex, OpenCode; OpenCode itself fronts many providers), not by a
client of our own.

Alternatives considered and not taken, recorded for context:

| Option | Build cost | Control | Outcome |
|---|---|---|---|
| Headless CLIs behind a trait | Low: one small adapter each | Coarse: prompt, tools, limits, model | **Chosen.** All three CLIs are installed here; edit tools, compaction and permissioning come free |
| Own Rust loop + LLM client | High: tool set, edit format, compaction, per-provider quirks, ongoing upkeep | Full | Not taken. Could become another `AgentRuntime` impl if evals ever demand it |
| Embed or fork goose | Medium to start, high to maintain a fork | High | Not taken. Large dependency surface and its own recipe model competing with §6 |

Accepted costs of the decision: transcripts and token accounting differ
per CLI, and mid-turn steering is not possible. Both are tolerable for
one-shot runs.

## 5. Isolation and devboxes

**Recommendation (pending user confirmation): fixed git-worktree slots,
with the agent and every verify command wrapped in Anthropic's `srt`
sandbox runtime from phase 1.** A VM backend is the phase-4 hard
boundary. All of it sits behind the `Sandbox` trait, so the layers above
(blueprints, supervisor, run log, hydration) do not change if the
backend does.

```rust
// ohara-harness::sandbox
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn acquire(&self, base: &BaseRef, branch: &BranchName) -> Result<SandboxLease>;
    async fn exec(&self, lease: &SandboxLease, command: &CommandSpec) -> Result<ExecOutput>;
    async fn release(&self, lease: SandboxLease, keep: KeepPolicy) -> Result<()>;
}
```

`AgentRuntime` launches its CLI through `Sandbox::exec`, so the agent
process and the verify commands get the same confinement.

### Slots

```
~/.ohara/<repo-id>/
├── index.sqlite              existing; shared by all slots (§3)
├── harness.sqlite            new (§6, §8)
├── slots/<n>/
│   ├── worktree/             git worktree of the user's clone, fixed path
│   └── cache/                build cache (e.g. CARGO_TARGET_DIR), fixed path
└── runs/<run-id>/            context.md, step transcripts, verify logs
```

- **Slots are the unit of concurrency.** `max_concurrent_runs` (default 2)
  is the number of slots. Slot ownership is a row in `harness.sqlite`,
  claimed in FIFO order (§6).
- **Slot paths are fixed and each slot reuses its own cache.** Cargo
  fingerprints absolute paths, so copying a `target/` to a new path
  forces a rebuild. Caches are never copied between slots or created per
  run.
- **Lifecycle.** `acquire`: `git fetch`, then in the slot worktree
  `checkout --detach <base>`, `clean -fdx`, `checkout -b ohara/<run-id>-<slug>`.
  The cache directory sits outside the worktree so cleaning never touches
  it. `release`: detach HEAD and free the slot; the branch stays in the
  user's clone until `ohara runs clean` removes branches of merged or
  abandoned runs.
- **Pre-warming.** `ohara harness warm` runs the repo's configured `warm`
  command (for this repo, `cargo build --workspace --tests`) in every free
  slot at the current base, and again after a run releases a slot. A run
  then starts from an incremental build: the laptop equivalent of
  Stripe's 10-second devbox.
- **Git and `gh` are shelled out** (`git worktree`, `git push`,
  `gh pr create`) through one `CommandSpec` runner, outside the sandbox,
  so the user's credential helpers work.

### Phase 1 confinement: `srt`

`srt` applies Seatbelt on macOS and bubblewrap on Linux, plus a proxy
that allows network access by domain. `SrtSandbox` wraps
`WorktreeSandbox` and generates one policy per run:

- **Write:** the slot worktree, the slot cache, the run directory, and
  inside the shared `.git` only what a commit needs (objects, the slot's
  `worktrees/<name>` entry, the run's own `refs/heads/ohara/<run-id>-*`
  and its reflog). Hooks, `config` and all other refs are read-only.
- **Deny read:** `~/.ssh`, `~/.config/gh`, `~/.aws` and similar
  credential paths, Keychain files, `harness.sqlite`.
- **Network:** default deny; allowlist the LLM endpoint and the
  registries named in `harness.toml`.
- **Daemon:** the daemon's Unix socket is allowed. `ohara-mcp` inside the
  sandbox runs daemon-only (no in-process fallback, which would need
  write access to `index.sqlite`'s WAL files); the supervisor makes sure
  the daemon is up first.
- **Nested Seatbelt.** A Seatbelt sandbox cannot start another one. When
  `srt` wraps the agent, the agent CLI's own sandbox is disabled
  (`codex exec --sandbox danger-full-access`, Claude Code's sandbox
  off); `srt` is then the only enforcement, which is also what gives
  Codex its command restrictions (§4).

`srt` is an npm package, so it adds a Node runtime requirement that
`ohara harness doctor` checks. Its policy format and version are pinned
in the plan. Running without it (`--sandbox worktree`) is possible for
debugging and prints a warning.

A process sandbox is not a hard boundary: it shares the kernel and the
user account, and Seatbelt profiles are easy to get subtly wrong. §11
says what it does and does not stop.

### Phase 4 hard boundary: a VM

| Candidate | For | Against |
|---|---|---|
| **Tart** (macOS guests via Virtualization.framework) — leading | Native macOS guest, so CoreML and the existing `target/` work unchanged; fast clone of a warmed image | macOS guests are limited to 2 per host, which caps slots at 2; licence terms to confirm; large images |
| Apple `container` (Linux guests) | Light, fast start, OCI images | Linux guest: no CoreML, separate Linux build cache, virtiofs I/O cost on shared directories |
| libkrun (Linux microVMs) | Very light; also works on Linux hosts | Same Linux-guest limits; less mature tooling on macOS |

### Alternative: APFS clones instead of worktrees

`cp -cR` of the clone gives each slot its own `.git`, which removes the
shared-`.git` exposure entirely. The cost: such a slot is a separate
clone, so it does not share the index through `commondir` (§3) and needs
`ohara-mcp --repo`, and `git fetch` is needed to move commits between
slot and main clone. Worth it only if tampering with the shared `.git`
turns out to matter in practice.

## 6. Blueprints

**Recommendation: blueprints are Rust values composed from a fixed set of
step kinds; per-repo behaviour comes from a TOML config file, not from a
workflow language.** A blueprint is a short linear sequence with one
bounded loop. A DAG engine or YAML DSL would be an extension point with a
single user.

```rust
// ohara-harness::blueprint
pub enum Step {
    Deterministic(Box<dyn Node>),            // prepare, hydrate, guard, format, push, open_pr
    Agent(AgentStep),                        // prompt template + ToolPolicy + limits
    FixLoop {
        verify: Box<dyn Node>,
        fix: AgentStep,
        settle: Vec<Box<dyn Node>>,          // re-run after every fix round
        max_rounds: FixRounds,
    },
}
pub struct Blueprint { name: BlueprintName, version: u32, steps: Vec<Step> }
```

`Node::run(&self, context: &mut RunContext) -> Result<NodeOutcome>` where
`NodeOutcome` is `Passed | Failed { summary } | Abort { reason }`.
Nodes do not call each other; only the `Supervisor` sequences them (the
same rule as the indexer's `Coordinator`, CONTRIBUTING §1).

The one phase-1 blueprint, `implement`:

| # | Step | Kind | Notes |
|---|---|---|---|
| 1 | `prepare` | deterministic | claim slot, branch from base, `EnsureIndexed { base_sha }` (§3) |
| 2 | `hydrate` | deterministic | build `context.md` (§7) |
| 3 | `implement` | agent | may `git add`/`commit`, may not push |
| 4 | `settle` | deterministic | `guard` (non-empty diff; commit leftovers; reject edits to protected globs; diff size cap), then `format` (autofixers such as `cargo fmt --all`, commit if dirty), then a fire-and-forget `IndexTip` |
| 5 | `verify` ⟲ `fix` → `settle` | fix loop | lint then test. On failure: fix agent, then `settle` again, then verify again. `max_fix_rounds = 2` |
| 6 | `guard` | deterministic | final check on the exact tree about to be pushed |
| 7 | `push` | deterministic | push the run branch only |
| 8 | `open_pr` | deterministic | `gh pr create --draft` with task link, summary, verify result |

`guard` and `format` therefore run after *every* agent step, not only
after `implement`: a fix round can touch a protected path or leave
unformatted code just as easily. Lint and test share one fix-round
budget; a round spent on a lint failure is a round not available for a
test failure.

If the fix loop exhausts its rounds, the run ends `needs_human`. By
default the branch is still pushed and a draft PR opened with the failing
output in the body, since partial work plus a precise failure is usually
worth more than nothing (open question 6).

### Per-repo configuration

`.ohara/harness.toml`, checked in, read from the base commit:

```toml
[verify]
format = ["cargo fmt --all"]
lint   = ["cargo clippy --workspace --all-targets -- -D warnings"]
test   = ["cargo test --workspace"]
max_fix_rounds = 2

[sandbox]
warm = ["cargo build --workspace --tests"]
cache_env = { CARGO_TARGET_DIR = "{slot_cache}" }
max_concurrent_runs = 2

[agent]
runtime = "claude-code"
model = ""                      # empty = the CLI's default

[guard]
protected = [".ohara/**", ".github/workflows/**"]
max_changed_lines = 1500
```

### Run log and resume

`harness.sqlite`, written through a new narrow trait (not by widening
`ohara_core::Storage`, which already has ~30 methods):

```rust
// ohara-core::run
#[async_trait]
pub trait RunStore: Send + Sync {
    async fn create_run(&self, run: &RunRecord) -> Result<()>;
    async fn set_run_state(&self, id: &RunId, state: RunState, detail: Option<&str>) -> Result<()>;
    async fn record_step(&self, step: &StepRecord) -> Result<()>;
    async fn get_run(&self, id: &RunId) -> Result<Option<RunRecord>>;
    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>>;
    async fn list_steps(&self, id: &RunId) -> Result<Vec<StepRecord>>;
}
```

```sql
CREATE TABLE run (
  id TEXT PRIMARY KEY,              -- ULID (ulid is a workspace dep)
  repo_id TEXT NOT NULL,
  blueprint TEXT NOT NULL, blueprint_version INTEGER NOT NULL,
  task_kind TEXT NOT NULL,          -- prompt | issue | pr | url
  task_ref TEXT NOT NULL, task_title TEXT NOT NULL,
  state TEXT NOT NULL,              -- queued | running | succeeded | needs_human | failed | cancelled
  base_sha TEXT NOT NULL, branch TEXT NOT NULL, slot INTEGER,
  runtime TEXT NOT NULL, model TEXT NOT NULL,
  supervisor_pid INTEGER,           -- for signalling only, never for liveness
  heartbeat_at INTEGER,             -- written by the supervisor every ~10 s
  pr_url TEXT, error TEXT,
  eval_case_id TEXT,                -- set for eval replays (§10)
  created_at INTEGER NOT NULL, finished_at INTEGER
);
CREATE TABLE run_step (
  run_id TEXT NOT NULL REFERENCES run(id),
  seq INTEGER NOT NULL, attempt INTEGER NOT NULL,
  name TEXT NOT NULL, kind TEXT NOT NULL,       -- deterministic | agent
  state TEXT NOT NULL,                          -- running | passed | failed | aborted
  started_at INTEGER NOT NULL, finished_at INTEGER,
  exit_code INTEGER, summary TEXT, artifact_path TEXT,
  head_sha TEXT,                                -- branch tip after the step
  tokens_in INTEGER, tokens_out INTEGER, cost_usd REAL,
  PRIMARY KEY (run_id, seq, attempt)
);
```

Resume re-enters at the first step without a `passed` row. Deterministic
steps are written to be idempotent (`prepare` re-checks out the existing
branch; `push` and `open_pr` check for an existing remote branch and PR).
An interrupted agent step restarts from the branch tip recorded by the
previous step (`git reset --hard <head_sha>`), so half-finished edits are
discarded rather than trusted.

- **Connection settings.** `harness.sqlite` is opened through the
  existing `SqlitePoolBuilder` (WAL, `busy_timeout`), and every write
  transaction starts with `BEGIN IMMEDIATE`, because several supervisor
  processes write the same file and a deferred transaction that upgrades
  to a write can fail with `SQLITE_BUSY` regardless of the timeout.
- **Liveness is a heartbeat, not a pid.** Pids are reused, so a bare pid
  can make a dead run look alive. A `running` run whose `heartbeat_at` is
  older than 60 s is shown as `interrupted` and its slot can be
  reclaimed. (Pid plus process start time would also work but needs
  `unsafe` libc calls, which the workspace forbids outside a dedicated
  module.) `cancel` signals `supervisor_pid` only while the heartbeat is
  fresh.
- **Slot claims are FIFO.** A queued run polls; inside one
  `BEGIN IMMEDIATE` transaction it takes a free slot only if it is the
  oldest `queued` run (ULID order). File-lock acquisition order is not
  fair, so slot ownership lives in the `run.slot` column, not in an
  `fs2` lock.

## 7. Context pre-hydration

`hydrate` is deterministic and makes no LLM call. It assembles
`runs/<run-id>/context.md` from ordered sections, each with a cap, and
prepends it to the implement prompt. The file is kept for audit and for
eval diffing.

| Priority | Section | Source | Default cap (tokens) |
|---|---|---|---|
| 1 | Task | prompt text, or `gh issue view` / `gh pr view --json` (title, body, labels, comments) | 3,000 |
| 2 | Linked items | `#123` refs and URLs to the same repo found in the task, fetched one level deep via `gh` | 2,000 |
| 3 | Scoped rules | `.ohara/rules/*.md` whose `globs` frontmatter matches the candidate paths | 1,500 |
| 4 | Prior art | `find_pattern` over the task title and body, top 5 hits with commit message and diff excerpt | 3,500 |
| 5 | Why it looks this way | `explain_change` for each `path:line` reference in the task, and for the top prior-art file | 1,000 |
| 6 | Memories (phase 5) | `recall` over the task text | 1,000 |

- **Budget.** The caps sum to 11,000 tokens, 12,000 with memories; the
  total cap is 12,000. Tokens are estimated as bytes / 3, which
  over-counts prose slightly and is about right for code and diffs, so
  the estimate errs toward staying under budget. No tokenizer
  dependency. A section over its cap is truncated at item boundaries and
  the omission is stated in-line so the agent can fetch the rest just in
  time via tools. Unused budget is not redistributed.
- **Fetch first, then assemble.** Rules (priority 3) are selected by
  candidate paths, which come partly from prior art (priority 4). So
  hydration has two passes: gather everything (task, links, prior art,
  explanations, then rules and memories), then write the sections in
  priority order.
- **Candidate paths** are the paths named in the task plus the files of
  the prior-art hits. They drive rule selection.
- **Rules.** Nested `CLAUDE.md` / `AGENTS.md` files are already loaded by
  the agent CLIs and are not duplicated. `.ohara/rules/` exists for
  glob-scoped rules those formats cannot express; the harness also points
  the agent at `CONTRIBUTING.md`-style standards named in config.
- **Retrieval path.** The supervisor calls the shared daemon through the
  existing `ohara_engine::client::{find_or_spawn_daemon, try_daemon_call}`
  with `RequestMethod::FindPattern` / `ExplainChange`, falling back to an
  in-process `RetrievalEngine` exactly as `ohara query` does. Queries
  need no new IPC methods.
- **No indexing here.** `prepare` has already made sure the base is
  indexed (§3). `hydrate` only queries, from the slot worktree, so
  results are filtered to commits visible from the run's HEAD.

Deliberately absent: an LLM "planning" pass over the context and
embedding-based code search of the current tree. Honk's result (few
tools, no code search) argues that the agent's own grep is enough; lineage
is the part it cannot get by grepping.

## 8. Toolshed and memory

### Toolshed

**Recommendation: extend `ohara-mcp`; no new server.** It is already a
thin client of the daemon and already the place tools are registered
(`crates/ohara-mcp/src/tools/`, the `#[tool(tool_box)]` block on
`OharaService`).

With the shared store (§3), `ohara-mcp` started in a slot worktree
resolves to the main clone's `RepoId` by itself, sees exactly the history
reachable from the run's HEAD, and can explain the run's own commits
(from the index once the background `IndexTip` has landed, from git
metadata before that). One change is required:

- **`--tools <a,b,c>`.** Restricts the advertised tool list. Per-step
  subsetting is then data: `AgentStep.tool_policy` names the ohara tools,
  the built-in agent tools and any extra MCP servers, and the adapter
  renders that into the CLI's MCP config and allow/deny flags.

`--repo <path>` / `OHARA_REPO` is kept only for slots that are separate
clones (the APFS alternative in §5), where `commondir` does not lead back
to the indexed clone. It is not needed for worktrees and not part of
phase 1.

Default subsets:

| Step | Built-in tools | ohara tools | Shell |
|---|---|---|---|
| `implement` | read, edit, search | `find_pattern`, `explain_change` (+ `recall` in phase 5) | allowlist from config; `git add/commit/diff/status`; no push, no `gh` |
| `fix` | read, edit, search | `explain_change` | same |

Third-party MCP servers are passed through from
`[[agent.mcp_servers]]` in `harness.toml` unchanged. We do not build a
500-tool registry; the subset mechanism is the part of Toolshed worth
copying.

### Memory (phase 5, an unproven bet)

Stripe runs without persistent memory and the ArcticMem numbers are
unverified, so memory ships last, off by default, and is evaluated with
§10 before it is ever enabled by default.

```sql
CREATE TABLE memory (
  id TEXT PRIMARY KEY,                  -- ULID
  repo_id TEXT NOT NULL,
  text TEXT NOT NULL,                   -- one fact, one or two sentences
  scope_glob TEXT,                      -- optional path scope
  state TEXT NOT NULL,                  -- candidate | active | superseded | rejected
  created_at INTEGER NOT NULL,
  superseded_by TEXT REFERENCES memory(id), superseded_at INTEGER,
  source_run_id TEXT REFERENCES run(id), source_step TEXT,
  reviewed_at INTEGER, embedding_model TEXT NOT NULL
);
-- plus vec_memory (sqlite-vec) and fts_memory (FTS5), same pattern as vec_hunk / fts_hunk
CREATE TABLE memory_event (             -- audit of every consolidation decision
  id INTEGER PRIMARY KEY, memory_id TEXT, run_id TEXT,
  decision TEXT NOT NULL,               -- add | update | delete | noop
  rationale TEXT, at INTEGER NOT NULL
);
```

- **Write path.** A final, non-blocking `consolidate` step runs after the
  PR is open. One agent call extracts candidate facts from the transcript
  and verify failures; for each, the nearest existing memories are
  recalled and the model picks add / update / delete / no-op. Update and
  delete never mutate a row: they insert a successor and set
  `superseded_by`. Failure of this step never changes the run's state.
- **Human review.** New rows are `candidate` and are not recalled until
  approved with `ohara memory review` (list, approve, reject, edit).
  `ohara memory list --all` shows history including superseded rows.
- **Read path.** `recall` fuses `vec_memory` KNN and `fts_memory` BM25
  with the existing `ohara_core::query::reciprocal_rank_fusion`, filtered
  to `state = 'active'` and matching `scope_glob`. Embeddings come from
  the existing `EmbeddingProvider`; `embedding_model` is stamped so a
  model change triggers re-embedding rather than silent mismatch.
- **Tools.** `recall` and `remember` on `ohara-mcp`. `remember` only
  inserts `candidate` rows, which preserves the spirit of the v1
  "MCP server is read-only against the index" invariant.
- Trait: `MemoryStore` in `ohara-core::memory`, implemented beside
  `RunStore`.

## 9. Run orchestration and surfaces

**Recommendation: one supervisor process per run; no new daemon, and runs
are not hosted in `ohara serve`.** The daemon exits after 1,800 s
idle, and a newer-version daemon sends `Shutdown` to older ones on start
(plan-29 version sweep). Either would kill a 40-minute run. A supervisor
process whose whole state is in SQLite and the worktree needs neither
socket nor registry. A daemon restart mid-run costs the supervisor
nothing: the next query or `IndexTip` finds or spawns a daemon as usual.

- `ohara run` creates the `run` row, then either runs the supervisor in
  the foreground (phase 1) or re-executes itself detached (`--detach`,
  phase 2) and returns the run id.
- Concurrency is the slot count (§5). Queued runs claim slots in FIFO
  order (§6); there is no scheduler process.
- Cancellation: `ohara runs cancel <id>` signals `supervisor_pid` (while
  its heartbeat is fresh); the
  supervisor kills the child process group, marks `cancelled`, releases
  the slot.

CLI (new files under `crates/ohara-cli/src/commands/`, one per
subcommand, as today):

| Command | Purpose |
|---|---|
| `ohara run "<prompt>"` / `--issue <n>` / `--pr <n>` / `<url>` | start a run; `--blueprint`, `--base`, `--detach`, `--no-pr`, `--runtime`, `--model` |
| `ohara runs` | list runs (state, task, step, duration, PR) |
| `ohara runs show <id>` | step table, costs, paths to artifacts |
| `ohara runs logs <id> [--step <name>] [--follow]` | transcripts and verify output |
| `ohara runs resume <id>` / `cancel <id>` / `clean` | lifecycle; `clean` removes finished run branches and prunes index rows for commits unreachable from any ref (§3) |
| `ohara harness init` / `warm` / `doctor` | write a starter `harness.toml`; warm slots; check `git`, `gh`, agent CLI, auth |
| `ohara eval` (phase 3) | §10 |
| `ohara memory …` (phase 5) | §8 |

PR creation: `gh pr create --draft --head ohara/<run-id>-<slug>`, body
generated from the run log (task link, what changed, verify result, fix
rounds used, run id). The repo's commit rules apply to agent commits and
are passed as rules (for this repo: Conventional Commits, no AI
attribution footers).

Notifications: a single optional `notify_command` in config, invoked with
run id, state and PR URL in the environment when a run finishes. No
built-in Slack or desktop integrations.

## 10. Verification and evals

### Verification

- **Format** is a deterministic autofix step; the agent never spends a
  turn on formatting.
- **Lint** runs before tests and should finish in seconds on a warm slot.
  Config may supply a `lint_changed` template that receives the changed
  files or crates (for this repo: `cargo clippy -p <changed crates>`).
- **Test selection** (phase 3): `[[verify.select]]` entries map path
  globs to commands (`crates/ohara-core/** → cargo test -p ohara-core`).
  Changed paths with no match fall back to the full `test` command.
  Phase 1 always runs the full command.
- **Fix rounds** are capped at 2 locally, shared between lint and test
  failures. `guard` and `format` re-run after each round (§6).
- **Failure output** handed to the fix agent keeps the **head** of each
  failing command's output (default 120 lines: the first errors, which
  are usually the cause) and the **tail** (default 60 lines: the
  summary), with a marker stating how much was elided. A plain tail
  would drop the first compiler error and keep only the cascade. The
  full log stays in the run directory for the agent to read on demand.
- **CI round** (phase 3, optional): after `open_pr`, wait on
  `gh pr checks`, and on failure run one more fix round from the failing
  job's log. Hard cap of 1, giving Stripe's "at most two CI rounds"
  including the first.

### Evals

`ohara eval` replays past tasks through a blueprint with `push` and
`open_pr` removed, in ordinary slots, recording results as `run` rows
with `eval_case_id` set.

- **Case format** (`.ohara/evals/cases.jsonl`, same JSONL habit as
  plan-10's `golden.jsonl`): `id`, `task` (prompt or frozen issue text),
  `base_sha`, `check` (commands that must pass, typically the tests the
  real fix added), `reference_sha` (the human commit, optional), `notes`.
- **Bootstrap.** `ohara eval import --pr <n>` drafts a case from a merged
  PR: base is the merge's first parent, task is the linked issue, check
  defaults to the tests touched by the PR. A human edits the draft; cases
  are curated, not mined in bulk.
- **Scoring.** Primary: `check` passes on the run's final tree (with the
  reference tests copied in, so the agent cannot weaken them). Secondary:
  verify green, fix rounds used, wall time, cost, changed-file overlap
  with the reference commit.
- **Output.** One summary line in the style of `perf::context_engine_eval`
  (`cases`, `pass_rate`, `verify_green_rate`, `mean_fix_rounds`,
  `p50_minutes`, `total_cost_usd`, `failed_ids`) plus a per-case table.
  Agent runs are non-deterministic, so `--repeat <n>` exists and
  comparisons should be read as rates over at least a dozen cases.
- Changes to prompts, hydration, tool subsets, blueprints or memory
  should quote the summary line in the PR, mirroring CONTRIBUTING §14.

## 11. Security

An unattended agent executing code derived from issue text is the main
risk. Controls, by phase:

| Control | Phase | Mechanism |
|---|---|---|
| PRs are always drafts; never merge; never push to the base branch | 1 | `push` only accepts `ohara/*` refs; no merge code exists |
| Push and PR are not available to the agent | 1 | deterministic nodes run them outside the sandbox; inside it there is no network path to the remote and no readable credential |
| Filesystem confinement | 1 | `srt` policy (§5): writes limited to the slot, its cache, the run directory and the run's own branch refs; deny-read on credential paths |
| Network policy | 1 | `srt` domain-allowlist proxy: LLM endpoint and configured registries only |
| Environment scrubbing | 1 | child processes get an allowlisted environment (PATH, HOME, toolchain vars, `cache_env`) |
| Config from base commit; protected globs | 1 | `guard`, after every agent step and before push, fails the run if the diff touches `.ohara/**`, CI workflows or other configured globs |
| Limits | 1 | per-step turn and wall-clock caps, per-run cost cap where the runtime reports cost, diff size cap |
| Fetched text is data | 2 | hydration wraps issue and PR content in delimited blocks marked untrusted; only same-repo links are followed |
| Task source gate | 2 | `--issue` refuses issues not authored or labelled (`ready-for-agent`) by a repo collaborator, unless `--trust` |
| Hard boundary | 4 | VM backend (§5): agent and verify run in a guest with no host credentials present |

What these do **not** do, stated plainly:

- **Tool allowlists are advisory.** `ToolPolicy` limits which shell
  commands the agent may type, but the agent writes code that the
  harness then runs: a `build.rs`, a proc macro or a test body executes
  arbitrary code during `verify`, with whatever the process is allowed
  to do. In phases 1–3 the only thing constraining that code is the
  `srt` policy.
- **Environment scrubbing protects environment variables only.** It does
  nothing for the Keychain, `~/.ssh` or `~/.config/gh`, which any process
  running as the user can read. Those are protected by the `srt`
  deny-read rules and by nothing else.
- **`srt` is a process sandbox, not a VM.** It shares the kernel and the
  user account; a policy mistake or a sandbox escape exposes the host.
  Without `srt` (`--sandbox worktree`) there is no confinement at all.
- **The shared `.git` is partly writable** (objects and the run's refs).
  A run can create objects and move its own branch; it cannot edit hooks,
  config or other refs.

Phases 1–3 are therefore suitable for the user's own repos and for tasks
from people they trust. Tasks from untrusted sources wait for phase 4.

## 12. Crate layout

**Recommendation: one new library crate, `ohara-harness`.** Splitting
into `-blueprint`, `-sandbox`, `-memory` now would create crate
boundaries around modules that each have exactly one consumer.

```
ohara-core      + run.rs         RunId, RunState, RunRecord, StepRecord, RunStore
                + visibility.rs  CommitVisibility; Retriever post-filter; per-worktree tips on Storage
                + memory.rs      (phase 5) MemoryId, MemoryRecord, MemoryStore
ohara-git       + commondir resolution; impl CommitVisibility; multi-tip walker
ohara-storage   + migrations/V7__indexed_tip.sql, tables/indexed_tip.rs, prune
                + harness/       SqliteHarnessStore: impl RunStore (+ MemoryStore)
                + harness_migrations/V1__run_log.sql …
ohara-engine    + RepoHandle split (per-repo storage, per-worktree blamer),
                  AncestryCache, IPC: EnsureIndexed / IndexTip / PruneUnreachable,
                  daemon-side index queue
ohara-harness   (new)            task, config, blueprint/, nodes/, agent/, sandbox/,
                                 hydrate/, supervisor, eval/ ; trait AgentRuntime, Sandbox, Node
ohara-mcp       + --tools ; (phase 5) tools/recall.rs, tools/remember.rs
ohara-cli       + commands/run.rs, runs.rs, harness.rs, eval.rs, memory.rs
```

Dependency graph (arrows are "depends on"):

```
ohara-cli ──▶ ohara-harness ──▶ ohara-core ◀── ohara-storage
    │               └─────────▶ ohara-engine (retrieval client only)
    └──▶ ohara-storage, ohara-engine   (wires SqliteHarnessStore into the Supervisor)
```

- `ohara-harness` depends on `ohara-core` for `RunStore` and on
  `ohara-engine` for the daemon client; it does **not** depend on
  `ohara-storage`. The CLI injects `Arc<dyn RunStore>`.
- Store traits live in `ohara-core` because that is where `Storage`
  lives and CONTRIBUTING §1 has leaves implementing core-owned traits.
  Alternative: own them in `ohara-harness` and have `ohara-storage`
  depend on it; rejected as a leaf-to-library edge that §1 says must be
  justified.
- `CommitVisibility` follows the same direction as `Storage` and
  `BlameSource`: trait in `ohara-core`, impl in `ohara-git`, wired by
  `ohara-engine`. No new edges between leaf crates.
- `AgentRuntime` and `Sandbox` adapters are modules inside
  `ohara-harness` (subprocess wrappers, no heavy dependencies).
- Typed errors: `ohara_harness::Error` via `thiserror`. Newtypes for
  `RunId`, `BranchName`, `SlotIndex`, `BlueprintName`, `ModelName`,
  `FixRounds`, `TokenBudget`.

Rules this design bends or extends, to be confirmed:

- **V7 migration** (`indexed_tip`) is an ordinary append-only migration
  in `crates/ohara-storage/migrations/`; no rule is bent. The daemon
  gaining an index write path revises the v1 statement that indexing
  happens only via the CLI; the v1 spec should get a dated note, as it
  did for plan-16.
- **Retrieval change.** The visibility filter is a ranking-path change
  and falls under CONTRIBUTING §14 (run the eval, quote the summary).
- **Second migration set.** CONTRIBUTING §9 says migrations live in
  `crates/ohara-storage/migrations/`. `harness.sqlite` is a different
  database and needs its own append-only set in
  `crates/ohara-storage/harness_migrations/` (a sibling directory, so
  `refinery::embed_migrations!("migrations")` does not pick it up). §9
  should gain a sentence saying so.
- **CONTRIBUTING §1 / §3 crate lists** gain `ohara-harness` as a library.
- **New dependencies:** `toml` (config), `globset` (rule and guard
  globs; already in the graph via `ignore`), and the `process` feature on
  the workspace `tokio` entry. `srt` is an external runtime tool, not a
  Cargo dependency.

## 13. Phasing

Each phase is one or more plans in `docs/superpowers/plans/` (next free
number is plan-33).

**Phase 1a — one store for all worktrees (prerequisite, §3).**
`commondir`-based `RepoId`, V7 `indexed_tip` with backfill, multi-tip
walker, `CommitVisibility` filter with LRU, `RepoHandle` split,
daemon-side indexing (`EnsureIndexed`, `IndexTip`, `PruneUnreachable`),
`explain_change` git-metadata fallback.
Acceptance:
- In a linked worktree of an indexed clone, `ohara status` and
  `find_pattern` work with no extra flags and no re-index; a second,
  separate clone still gets its own `RepoId`.
- An existing index opens after V7 with its tip backfilled and nothing
  re-embedded; the plain-clone `RepoId` is unchanged.
- A commit made on a branch in worktree A and indexed is returned by a
  query from A, and is not returned by a query from the main clone or
  from worktree B.
- Indexing worktree A after the main clone embeds only A's new commits
  (asserted on the embed count).
- `explain_change` on a just-made, not-yet-indexed commit returns git
  metadata marked as not indexed, and the indexed form once `IndexTip`
  has completed.
- The plan-10 eval summary is unchanged on the main clone; the
  visibility filter adds under 5 ms at p95 in `tests/perf`.

**Phase 1b — end-to-end MVP.** `ohara-harness` skeleton, `RunStore` +
`SqliteHarnessStore`, `WorktreeSandbox` with one slot and no pre-warm,
wrapped by `SrtSandbox`, `ClaudeCodeRuntime`, the hardcoded `implement`
blueprint, `harness.toml` (`verify`, `agent`, `guard`, `sandbox`),
minimal hydration (task + `find_pattern` top 5), `ohara run` in the
foreground, `ohara runs`, `runs show`, `runs logs`, `runs resume`.
Acceptance:
- `ohara run --issue <n>` on this repo ends with a draft PR URL printed
  and a `succeeded` row, with fmt, clippy and tests green in the slot.
- A task rigged to fail tests ends `needs_human` after exactly two fix
  rounds, visible in `ohara runs show`; `guard` and `format` rows appear
  after each round.
- A fix round that edits a protected path fails the run at `guard`.
- `kill -9` during `verify`, then `ohara runs resume <id>`, completes
  without re-running `implement`; while dead, the run shows as
  `interrupted` within 60 s.
- The user's own checkout and branch are untouched (`git status`
  identical before and after).
- With the base already indexed, the run log shows no blocking index
  step and the run triggers no embedding before the agent's first
  token; with a stale base, exactly one incremental index is recorded.
- Inside a run, writing outside the slot fails, reading `~/.ssh` fails,
  and an HTTP request to a host not on the allowlist fails, each
  asserted by an integration test (including from a `build.rs`).
- Supervisor logic is covered by tests using a scripted fake
  `AgentRuntime` and fake `Sandbox`; no network in CI.

**Phase 2 — context, toolshed, unattended operation.** Full hydration
sections 1–5 with budgets, `.ohara/rules/`, `ohara-mcp --tools` and
per-step `ToolPolicy`, `--detach`, N slots with FIFO claims, `ohara harness
warm` / `doctor` / `init`, `runs cancel` / `clean`, `notify_command`,
untrusted-content wrapping, task source gate.
Acceptance: three runs started back to back with two slots show two
`running` and one `queued`, and queued runs start in submission order;
two parallel runs from the same base embed that base zero additional
times; warm-slot time to first agent token under
15 s; `context.md` for a fixture task is byte-stable across two runs
and within budget; a step configured without `find_pattern` cannot call
it.

**Phase 3 — evals, smarter verify, second runtime.** `ohara eval` with
`import`, case format and summary line; test selection; `lint_changed`;
optional CI round; `CodexRuntime` (then `OpenCodeRuntime`).
Acceptance: an initial set of at least 12 cases from this repo's merged
PRs; `ohara eval` prints the summary line and exits non-zero below a
configured pass rate; the same set runs under both runtimes by changing
only `--runtime`.

**Phase 4 — hard boundary.** A VM-backed `Sandbox` (Tart is the leading
candidate, §5) so agent and verify code never run on the host.
Acceptance: agent and verify processes run in the guest; no host
credential path exists in the guest; warm start stays under 30 s; the
phase-3 eval pass rate does not drop. This phase moves ahead of phase 3
if tasks from untrusted sources are wanted sooner.

**Phase 5 — memory.** Schema, `MemoryStore`, `consolidate` step,
`recall` / `remember` tools, `ohara memory review`, hydration section 6.
Acceptance: candidate memories appear after runs and are never recalled
before approval; superseding a memory leaves the old row readable with
`superseded_by` set; eval pass rate with memory on is reported against
memory off, and memory stays off by default unless it wins.

## 14. Performance and risks

Performance notes:

- **Indexing cost does not scale with the number of runs.** Slots share
  one index; the base is embedded once on the main clone, ahead of time;
  each run adds only its own handful of commits, embedded in the
  background by the daemon.
- **No run waits on indexing** except the one-off incremental catch-up
  in `prepare` when the base is not yet covered, which is recorded in
  the run log.
- **The visibility filter adds a few milliseconds per query** (one
  ancestry check per candidate, LRU-cached, skipped entirely when no
  unmerged tips are indexed).
- **Single writer.** All index writes queue on the daemon's one write
  connection, so parallel runs cannot contend on SQLite's busy handler.

| Risk | Mitigation |
|---|---|
| Headless CLI flags and event formats change between releases | adapters are small; `ohara harness doctor` checks versions; recorded-transcript tests per adapter |
| Warm caches are large (a Rust target dir per slot) | slot count is small and configurable; `ohara runs clean --caches` |
| Runs are non-deterministic, so evals are noisy | rates over a curated set, `--repeat`, cost and time reported beside pass rate |
| `srt` policy too tight for a toolchain (blocked registry, cache path) or too loose | policy generated from `harness.toml`; phase-1 integration tests assert both the denials and a full green verify |
| Visibility over-fetch returns fewer than k hits when many indexed commits are invisible | bounded over-fetch with one retry at a larger pool; `runs clean` prunes dead branches |
| Background `IndexTip` lags, so a fix round cannot find the run's own commits in the index | by design: the run's own diff is in the worktree, and `explain_change` falls back to git metadata |
| Run branches accumulate index rows | `ohara runs clean` prunes commits unreachable from any ref |
| Head-snapshot symbol lane reflects the main clone, not the run | accepted (§3) |
| Scope creep toward a workflow engine | blueprints stay Rust until a third real blueprint exists |
| Agent-written build and test code runs during verify | confined by `srt` in phases 1–3, by a VM in phase 4; limits stated in §11 |

## 15. Open questions for the user

Resolved since the first draft and no longer open: the agent runtime
approach (§4), one store for all worktrees (§3), and indexing kept off
the run's critical path (§3).

1. **Runtime order.** Claude Code first and Codex second, or the other
   order?
2. **Phase-1 sandbox.** Confirm worktree slots wrapped in `srt` as the
   phase-1 backend, including its Node runtime requirement. Is a
   process sandbox enough for the tasks you plan to run first, or should
   the VM backend (phase 4, Tart leading, at most 2 slots) move ahead of
   evals?
3. **Where slots live.** The design puts worktrees under
   `~/.ohara/<repo-id>/slots/`. Would you rather they sit under
   `~/Documents/trees/<repo>/` like your other worktrees (a
   `worktree_root` setting)? Paths must stay fixed per slot either way.
4. **Separate `harness.sqlite`** with its own migration directory, which
   needs a one-line amendment to CONTRIBUTING §9 — acceptable? The
   alternative (tables in `index.sqlite`) loses run history and memories
   on every `ohara index --rebuild`.
5. **Foreground `ohara index` and the daemon.** Is it acceptable that an
   incremental `ohara index` routes through a running daemon, and that
   `--rebuild` refuses while runs are active?
6. **When fix rounds are exhausted:** open a draft PR marked as failing
   (the default here), or push nothing and only report locally?
7. **Task sources beyond GitHub.** Is `gh` (issues, PRs) plus free-text
   prompts enough for now, or do Jira/Linear links need hydration in
   phase 2?
8. **Budgets.** Default caps per run for wall time, agent turns and
   cost — any hard numbers you want (for example 45 minutes, 5 USD)?
9. **Config location and name.** `.ohara/harness.toml` and
   `.ohara/rules/` checked into the repo, next to the existing top-level
   `.oharaignore` — or a different layout?
10. **Memory review surface.** SQLite rows gated by `ohara memory review`
    (the design), or a checked-in markdown file reviewed through normal
    PRs? The second is simpler and more visible but gives up vector
    recall.
11. **Eval seed set.** Which merged PRs or closed issues from this repo
    should seed the first dozen eval cases, and is it acceptable for an
    eval sweep to spend real model budget on each run?
