# Outcome harness, phase 1: contract and clock (plan-34)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ohara task "<outcome>" --for 90m` turns an outcome into a contract
the user approves. A supervisor then runs agent CLIs in a loop until the
frozen contract passes or the clock runs out, records every unsupervised
decision, and hands back stacked branches with a review page.

**Architecture:**
- A new library crate, `ohara-harness`, owns the supervisor state machine,
  the agent-runtime adapters, contracts, the guard and the hand-back.
- The task and decision types and the `TaskStore` trait live in
  `ohara-core`. The SQLite implementation lives in `ohara-storage` and
  writes to a second database, `$OHARA_HOME/harness.sqlite`, with its own
  append-only migration set.
- `ohara-cli` wires the concrete store into the supervisor and adds the
  `ohara task` command.
- `ohara-mcp` gains one harness-only tool, `propose_decision`, which writes
  *proposals* that the supervisor later validates.

**Tech stack:** Rust stable, tokio (`process` feature), rusqlite and
refinery, clap, serde with the new `toml` dependency, `globset` (new
workspace entry; it is already in the graph via `ignore`), `blake3` and
`ulid` (both already workspace deps). Agent CLIs: `claude` 2.1.x, `codex`
0.163.x and `agy` 1.5.x, all shelled out.

**Spec:** `docs/superpowers/specs/2026-11-20-ohara-outcome-harness-design.md`.
Phase 1 is §15.1, and the sections it draws on are §4–§6, §8a, §9, §12 and
§13. Where the outcome spec says the earlier spec still holds, that is
`docs/superpowers/specs/2026-11-20-ohara-harness-design.md` §4 (agent
runtime) and the §6 run log.

## Global Constraints

- `CONTRIBUTING.md` is binding. In particular:
  - no `else`
  - no `unwrap`/`expect`/`panic!` outside tests, except
    `expect("invariant: …")`
  - library crates use `thiserror`; only binaries use `anyhow`
  - all SQL lives in `ohara-storage`
  - migrations are append-only
  - files stay under 500 non-test lines
  - dependencies are declared in the workspace only
  - newtypes for meaningful primitives
  - no `println!` outside `ohara-cli` user output
- `ohara-core` MUST NOT depend on any leaf crate. `ohara-harness` depends on
  `ohara-core` (and `ohara-engine` only if a task below says so), never on
  `ohara-storage`.
- Commits: Conventional Commits, scoped by crate. A red test commit, then a
  green commit. **No attribution footers of any kind.**
- **Each slice is one PR**, stacked on the previous one. Every PR passes
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` and `cargo test --workspace` on its own.
- **No sandbox in phase 1.** Agents run with the user's permissions in
  plain git worktrees (spec §15.1). The CLI prints this once per task at
  approval.
- **The contract is frozen at approval.** Every check run verifies the
  contract hash. Agents can never edit the contract or protected paths
  (spec §6).
- **Runtimes are set in config.** The harness never chooses a runtime on
  its own (spec §8a).
- **Paths:**
  - `$OHARA_HOME/harness.sqlite` (one database for all tasks; resolve the
    home with `ohara_core::paths::ohara_home`)
  - `$OHARA_HOME/config.toml` (runtimes, roles)
  - `$OHARA_HOME/workflows/<name>.toml`
  - `$OHARA_HOME/tasks/<task-id>/` (transcripts, logs, `review.html`)
  - per-task worktree at `$OHARA_HOME/tasks/<task-id>/worktree`
- **Contract side ref:** `refs/ohara/tasks/<task-id>/contract`.
- **Task branch:** `ohara/<task-id-lowercase>`. **Increment branches:**
  `ohara/<task-id-lowercase>/<n>`, where n starts at 1.
- **Liveness is a heartbeat, not a pid.** The heartbeat is written every
  10 s. A `running` task whose heartbeat is older than 60 s is shown as
  `interrupted` and can be resumed.

## Review Focus

1. **The deadline passes in the middle of an agent step.** The step is
   killed within 5 s of the deadline. The task ends `timed_out` with its
   last checkpoint delivered, not left half-applied. *(Task 13 test
   `deadline_kills_running_step_and_hands_back_last_checkpoint`.)*
2. **An agent edits the contract file or a protected path, then claims
   success.** The guard rejects the round and resets to the checkpoint,
   and the task can't reach `succeeded`. *(Task 9 test
   `edit_to_protected_path_is_rejected_and_reset`.)*
3. **A test script "passes" with zero tests run**, for example a filter
   matching nothing or `cargo test` in an empty crate. The script counts
   as failed. *(Task 8 test `zero_tests_executed_fails_the_script`.)*
4. **The supervisor process is killed (laptop sleep, Ctrl-C).**
   `ohara task resume <id>` continues from the last checkpoint and
   doesn't redo committed rounds. *(Task 13 test
   `resume_after_crash_restarts_from_last_checkpoint`.)*
5. **The primary runtime is rate-limited or not logged in.** The step
   fails over to the next runtime in that role's list. The failover is
   recorded, and the task doesn't fail. *(Task 5 test
   `rate_limited_primary_fails_over_and_records_it`.)*

---

## Slices → PRs

| PR | Slice | Tasks | Delivers |
|---|---|---|---|
| 1 | Store | 1, 2 | Task and decision types and `TaskStore` in core; `harness.sqlite` with its migrations and `SqliteTaskStore`; CONTRIBUTING amendment |
| 2 | Runtime core | 3, 4, 5 | `ohara-harness` crate, config and roles, the `AgentRuntime` trait, the generic adapter, the failover runner |
| 3 | `claude` adapter | 6 | Claude Code adapter |
| 4 | `codex` adapter | 7a | Codex adapter |
| 5 | `agy` adapter | 7b | agy adapter |
| 6 | Contract | 8 | Contract model, hash, side ref, check runner (required tests, zero-test guard) |
| 7 | Guard | 9 | Protected paths and test-weakening detection |
| 8 | Decisions | 10 | `propose_decision` MCP tool and supervisor validation |
| 9 | Drafting & approval | 11, 12 | Contract drafting, workflows with starters, `ohara task` and `approve` |
| 10 | Supervisor loop | 13 | Round loop, clock and cost ceiling, heartbeat, resume, stop |
| 11 | Hand-back | 14, 15 | Increments as stacked branches and draft PRs, summary, `review.html`, `ohara task review` |

---

### Task 1: Core types and the `TaskStore` trait

**Files:**
- Create: `crates/ohara-core/src/task.rs`, `crates/ohara-core/src/task/decision.rs`
- Modify: `crates/ohara-core/src/lib.rs` (add `pub mod task;` and re-exports)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Produces, in `ohara_core::task`:
  - `TaskId(Ulid)` with `new()`, `Display` and `FromStr`
  - `TaskState { Drafting, AwaitingApproval, Running, Succeeded, TimedOut, Aborted, Stopped }`
    with `as_str()` and `FromStr`
  - `Outcome(String)`
  - `Deadline(chrono::DateTime<Utc>)`
  - `CostCents(u64)`
  - `ContractHash([u8; 32])`, displayed as hex
  - `RuntimeName(String)`
  - `TaskRecord { id, outcome, workflow: String, repo_root: PathBuf, base_sha: String, branch: String, state, deadline, cost_ceiling: Option<CostCents>, cost_spent: CostCents, contract_hash: Option<ContractHash>, heartbeat_at: Option<DateTime<Utc>>, round: u32, created_at, finished_at: Option<_>, detail: Option<String> }`
  - `StepRecord { task_id, round, seq: u32, role: Role, runtime: RuntimeName, model: Option<String>, state: StepState, started_at, finished_at: Option<_>, head_sha: Option<String>, cost: Option<CostCents>, failover_from: Option<RuntimeName>, transcript: PathBuf, summary: Option<String> }`
  - `Role { Contract, Planner, Builder, Judge, Reviewer }` (phase 1 uses `Contract` and `Builder`)
  - `StepState { Running, Passed, Failed, Aborted }`
- Produces, in `ohara_core::task::decision`:
  - `DecisionId(Ulid)`
  - `DecisionState { Proposed, Settled, Rejected, Reopened, Superseded }`
  - `Assumption { text: String, check: Option<Vec<String>> }` (`check` is an argv)
  - `DecisionRecord { id, task_id, round, parent_ids: Vec<DecisionId>, question, options: Vec<String>, chosen: String, runner_up: Option<String>, assumptions: Vec<Assumption>, cost_if_wrong: CostIfWrong, checkpoint_sha: Option<String>, evidence: Evidence, state, rejected_reason: Option<String>, human_verdict: Option<Verdict>, verdict_note: Option<String> }`
  - `CostIfWrong { Low, Medium, High }`
  - `Verdict { Upheld, Overturned }`
  - `Evidence { script_results: Vec<ScriptResult>, runtime: Option<RuntimeName> }` (phase 1 has no judge, so judge fields come later as an additive change)
  - `ScriptResult { name: String, passed: bool, exit_code: Option<i32>, tests_run: Option<u32>, missing_required: Vec<String>, log: PathBuf }`
- Produces the trait `ohara_core::task::TaskStore` (async-trait, `Send + Sync`, errors via `ohara_core::Error`):
  ```rust
  async fn create_task(&self, t: &TaskRecord) -> Result<()>;
  async fn update_task(&self, t: &TaskRecord) -> Result<()>;
  async fn get_task(&self, id: &TaskId) -> Result<Option<TaskRecord>>;
  async fn list_tasks(&self) -> Result<Vec<TaskRecord>>;               // newest first
  async fn heartbeat(&self, id: &TaskId, at: DateTime<Utc>) -> Result<()>;
  async fn record_step(&self, s: &StepRecord) -> Result<()>;            // upsert on (task_id, round, seq)
  async fn list_steps(&self, id: &TaskId) -> Result<Vec<StepRecord>>;
  async fn put_decision(&self, d: &DecisionRecord) -> Result<()>;       // upsert on id
  async fn list_decisions(&self, id: &TaskId) -> Result<Vec<DecisionRecord>>;
  ```

- [ ] **Step 1: Write failing tests:** `task_id_round_trips_through_string`, `task_state_round_trips_every_variant`, `contract_hash_displays_as_64_hex_chars`, `decision_record_serde_round_trip`. All types derive `Serialize`/`Deserialize`, because `serde_json` stores the nested fields.
- [ ] **Step 2: Run** `cargo test -p ohara-core task::`. **Expected:** compile failure, because the types don't exist yet.
- [ ] **Step 3: Implement the types and the trait.** No SQL here.
- [ ] **Step 4: Run** `cargo test -p ohara-core task::`. **Expected:** PASS.
- [ ] **Step 5: Commit** the red and green commits separately: `test(core): …` and `feat(core): task and decision records and TaskStore`.

### Task 2: `harness.sqlite`: migrations and `SqliteTaskStore`

**Files:**
- Create:
  - `crates/ohara-storage/harness_migrations/V1__task_log.sql`
  - `crates/ohara-storage/src/harness/mod.rs`
  - `crates/ohara-storage/src/harness/migrations.rs`
  - `crates/ohara-storage/src/harness/task_store.rs`
- Modify:
  - `crates/ohara-storage/src/lib.rs` (`pub mod harness;`, re-export `SqliteTaskStore`)
  - `CONTRIBUTING.md` §9: add one sentence saying `harness.sqlite` has its own append-only set in `crates/ohara-storage/harness_migrations/`, embedded separately. Also add `ohara-harness` (Library) to the crate table.
- Test: `crates/ohara-storage/tests/harness_task_store.rs`

**Interfaces:**
- Consumes: Task 1's types and `TaskStore`.
- Produces: `SqliteTaskStore::open(path: &Path) -> Result<Self>` and `SqliteTaskStore::open_default() -> Result<Self>`, which opens `ohara_home()/harness.sqlite`. It implements `TaskStore`.
- Schema (V1): one table each for `task`, `step` and `decision`, with columns mirroring the Task 1 fields.
  - `parent_ids`, `options`, `assumptions` and `evidence` are JSON `TEXT`.
  - Times are unix milliseconds stored as `INTEGER`.
  - Primary keys: `task(id)`, `step(task_id, round, seq)`, `decision(id)`.
  - Index: `decision(task_id)`.
- Open through the existing `SqlitePoolBuilder` (WAL, `busy_timeout`). Every write transaction starts with `BEGIN IMMEDIATE`, because several supervisors write this file.
- Embed with `refinery::embed_migrations!("harness_migrations")` in its own module, so the index migration set is untouched.

- [ ] **Step 1: Write failing tests:**
  - `create_get_update_task_round_trip`
  - `list_tasks_newest_first`
  - `record_step_upserts_on_key`
  - `put_decision_upserts_and_lists_by_task`
  - `harness_migrations_do_not_touch_index_schema`: open `harness.sqlite` and assert there is no `commit_record` table.
  - `concurrent_writers_do_not_fail_busy`: two stores on one file, 50 interleaved `heartbeat` calls each, all `Ok`.
- [ ] **Step 2: Run** `cargo test -p ohara-storage --test harness_task_store`. **Expected:** FAIL.
- [ ] **Step 3: Implement** the migration, the embedding and the store.
- [ ] **Step 4: Run** the same command. **Expected:** PASS. Also run `cargo test -p ohara-storage`; everything stays green.
- [ ] **Step 5: Commit** (red, then green). **Open PR 1.**

### Task 3: The `ohara-harness` crate, config and roles

**Files:**
- Create:
  - `crates/ohara-harness/Cargo.toml`
  - `crates/ohara-harness/src/lib.rs`
  - `crates/ohara-harness/src/error.rs`
  - `crates/ohara-harness/src/config.rs`
- Modify: root `Cargo.toml`
  - add to workspace members
  - `toml = "0.8"`
  - `globset = "0.4"`
  - add `"process"` to tokio's features
  - add the `ohara-harness` path dependency
- Test: inline tests in `config.rs`

**Interfaces:**
- Produces `ohara_harness::Error` (thiserror) with these variants: `Config`, `Runtime`, `Contract`, `Guard`, `Git`, `Store(ohara_core::Error)`, `Io`, `Deadline`.
- Produces in `config`:
  - `HarnessConfig { runtimes: BTreeMap<RuntimeName, RuntimeSpec>, roles: RoleMap }`
  - `RuntimeSpec { kind: RuntimeKind, model: Option<String>, command: Option<Vec<String>>, prompt: Option<PromptMode>, done: Option<DoneRule> }`
  - `RuntimeKind { ClaudeCode, Codex, Agy, Generic }`, written in TOML as `"claude-code" | "codex" | "agy" | "generic"`
  - `PromptMode { Arg, Stdin }`
  - `DoneRule { exit_code: i32, output_contains: Option<String> }`
  - `RoleMap(BTreeMap<Role, Vec<RuntimeName>>)`
  - `HarnessConfig::load(path: &Path) -> Result<Self>` and `HarnessConfig::load_default() -> Result<Self>` (reads `ohara_home()/config.toml`)
- **Defaults when the file is missing:** one runtime `claude` (kind `claude-code`), and every role set to `["claude"]`.
- **Validation errors:**
  - a role lists a runtime that isn't defined
  - a `generic` runtime has no `command`
  - a role's list is empty

  Each error names the offending key.

- [ ] **Step 1: Write failing tests:**
  - `parses_spec_example_config`: the TOML block from spec §8a, verbatim.
  - `missing_file_yields_claude_default`
  - `role_referencing_unknown_runtime_is_error_naming_it`
  - `generic_without_command_is_error`
- [ ] **Step 2: Run** `cargo test -p ohara-harness config`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit** (red, then green).

### Task 4: The `AgentRuntime` trait, the process runner and the generic adapter

**Files:**
- Create:
  - `crates/ohara-harness/src/agent/mod.rs`
  - `crates/ohara-harness/src/agent/process.rs`
  - `crates/ohara-harness/src/agent/generic.rs`
  - `crates/ohara-harness/src/agent/fake.rs` (`#[cfg(any(test, feature = "test-util"))]`)
- Test: inline tests, plus `crates/ohara-harness/tests/fixtures/fake_agent.sh`

**Interfaces:**
- Produces:
  ```rust
  #[async_trait]
  pub trait AgentRuntime: Send + Sync {
      fn name(&self) -> &RuntimeName;
      async fn run(&self, req: AgentRequest) -> Result<AgentOutcome>;
  }
  pub struct AgentRequest { pub cwd: PathBuf, pub prompt: String, pub mcp_config: Option<PathBuf>,
      pub deadline: Deadline, pub transcript: PathBuf, pub model: Option<String> }
  pub enum AgentStatus { Completed, TimedOut, RateLimited, AuthFailed, Crashed }
  pub struct AgentOutcome { pub status: AgentStatus, pub final_message: String,
      pub cost: Option<CostCents>, pub exit_code: Option<i32> }
  ```
- Also produces `process::run_streaming(argv: &[String], stdin: Option<String>, cwd, deadline, transcript) -> Result<ProcessExit { code: Option<i32>, stdout_tail: String, killed_at_deadline: bool }>`.
  - It tees stdout and stderr into `transcript`.
  - At the deadline it sends SIGTERM, then SIGKILL 5 s later. Use `tokio::process::Command` with `kill_on_drop(true)`. Use the `nix`-free path: `child.start_kill()` for the kill, and accept SIGKILL-only if SIGTERM needs `unsafe`.
- `GenericRuntime::new(name, spec: &RuntimeSpec)`:
  - substitutes `{prompt}` in `command`, or pipes the prompt on stdin
  - maps `DoneRule` to `Completed`
  - reports any other exit as `Crashed`
  - always reports cost as `None`
- `classify_failure(stdout_tail: &str) -> AgentStatus`, shared by all adapters:
  - case-insensitive `"rate limit"`, `"429"`, `"quota"` → `RateLimited`
  - `"not logged in"`, `"unauthorized"`, `"401"`, `"login"` → `AuthFailed`
  - otherwise → `Crashed`
- `FakeRuntime`: a scripted queue of `AgentOutcome` values, with an optional closure that edits `cwd`. Later tasks use it.

- [ ] **Step 1: Write failing tests:**
  - `generic_arg_prompt_substitution`: the fixture script echoes its argv to a file; assert the prompt is there.
  - `generic_stdin_prompt`
  - `deadline_kills_child_within_5s`: the fixture sleeps 60 s with a deadline of now + 1 s; assert it returns in under 7 s with `TimedOut`.
  - `classify_rate_limit_and_auth_strings`
  - `transcript_contains_stdout`
- [ ] **Step 2: Run** `cargo test -p ohara-harness agent`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit** (red, then green).

### Task 5: Role runner with failover

**Files:**
- Create: `crates/ohara-harness/src/agent/roles.rs`
- Test: inline

**Interfaces:**
- Consumes: `HarnessConfig`, `AgentRuntime`, `TaskStore::record_step`.
- Produces `RoleRunner::new(config: &HarnessConfig, factory: impl Fn(&RuntimeName, &RuntimeSpec) -> Arc<dyn AgentRuntime>)` and:
  ```rust
  async fn run(&self, role: Role, req: AgentRequest, step: StepRecord, store: &dyn TaskStore)
      -> Result<(AgentOutcome, StepRecord)>
  ```
- Behaviour:
  - Try the role's runtimes in order.
  - Fail over on `RateLimited`, `AuthFailed` or `Crashed`.
  - Never fail over on `Completed` or `TimedOut`; a timeout means the clock ran out.
  - Before each retry, run `git reset --hard <step.head_sha>` in `req.cwd` through `Git` (Task 8 owns `Git`; until then this task takes a `reset: impl Fn() -> Result<()>` closure).
  - Record one `StepRecord` per attempt, with `failover_from` set on each retry.
  - When every runtime fails, return the last outcome.

- [ ] **Step 1: Write failing tests** using `FakeRuntime` and an in-memory `TaskStore` fake:
  - `rate_limited_primary_fails_over_and_records_it`: two step rows, the second with `failover_from = "claude"`.
  - `timed_out_does_not_fail_over`
  - `all_fail_returns_last_outcome`
- [ ] **Step 2: Run** `cargo test -p ohara-harness roles`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit.** **Open PR 2.**

### Task 6: The `claude` adapter

**Files:**
- Create:
  - `crates/ohara-harness/src/agent/claude.rs`
  - `crates/ohara-harness/tests/fixtures/claude_stream.jsonl`, recorded from a real `claude -p … --output-format stream-json --verbose` run of a trivial prompt
- Test: inline tests, plus `#[ignore]` live test `claude_live_smoke`

**Interfaces:**
- Produces `ClaudeCodeRuntime::new(name, model: Option<String>)`, which implements `AgentRuntime`.
- argv: `claude -p <prompt> --output-format stream-json --verbose --permission-mode acceptEdits --strict-mcp-config [--mcp-config <file>] [--model <m>]`, run in `cwd`. Builder steps need a shell, so add `--allowedTools "Bash Edit Write Read Glob Grep"`.
- **Verify first:** check every flag against `claude --help` on 2.1.329 and record the version in the module's doc comment.
- Parse stream-json lines:
  - the final `{"type":"result",…}` event gives `result` → `final_message` and `total_cost_usd` → `CostCents` (rounded up)
  - `is_error: true` → `classify_failure`

- [ ] **Step 1: Write failing tests:** `argv_has_strict_mcp_and_stream_json`, `parses_result_event_cost_and_message` (from the fixture), `error_result_classified`.
- [ ] **Step 2: Run** `cargo test -p ohara-harness claude`. **Expected:** FAIL.
- [ ] **Step 3: Implement.** Record the fixture with a real run.
- [ ] **Step 4: Run** the tests. **Expected:** PASS. Then run `cargo test -p ohara-harness claude -- --ignored`. **Expected:** the live smoke test PASSES.
- [ ] **Step 5: Commit.** **Open PR 3.**

### Task 7a: The `codex` adapter

Same shape as Task 6. Create `src/agent/codex.rs` and `tests/fixtures/codex_events.jsonl`.
- argv: `codex exec --json --skip-git-repo-check -C <cwd> --sandbox workspace-write [-m <model>] <prompt>`.
- The MCP config goes in through `-c mcp_servers.ohara.command=…` overrides. Verify that form against `codex exec --help` on 0.163.0, and record the version in the doc comment.
- The final message comes from the last agent-message event. Cost is `None` unless the events carry usage cost.
- **Tests:** `argv_shape`, `parses_final_message`, `error_event_classified`, plus the `#[ignore]` test `codex_live_smoke`.
- **Commit, then open PR 4.**

### Task 7b: The `agy` adapter

Same shape. Create `src/agent/agy.rs` and `tests/fixtures/agy_stream.jsonl`.
- argv: `agy -p=<prompt> --output-format stream-json --dangerously-skip-permissions [--model <m>]`.
- **Two known quirks, each with its own test:**
  - The prompt MUST be passed as the single token `-p=<prompt>`. A separate `-p <prompt>` swallows the next flag.
  - Headless mode auto-denies shell permissions, hence `--dangerously-skip-permissions`.
- MCP goes in through `--settings '{"mcpServers":{…}}'`. Verify this on 1.5.13. If agy can't take MCP servers per session, the adapter logs a warning and runs without `propose_decision` (the supervisor then records "decisions not narrated by agent").
- **Tests:** `prompt_is_single_equals_token`, `skips_permissions_flag_present`, `parses_result`, plus the `#[ignore]` test `agy_live_smoke`.
- **Commit, then open PR 5.**

### Task 8: Contract model, freeze and check runner

**Files:**
- Create:
  - `crates/ohara-harness/src/contract/mod.rs` (model, parse, hash)
  - `crates/ohara-harness/src/contract/freeze.rs` (side ref)
  - `crates/ohara-harness/src/contract/check.rs` (run scripts)
  - `crates/ohara-harness/src/contract/test_output.rs` (count tests, find required tests)
  - `crates/ohara-harness/src/git.rs` (shell-out runner)
- Test: inline tests, plus `crates/ohara-harness/tests/contract_freeze.rs` (a temp git repo)

**Interfaces:**
- Produces `Contract`, serialised as `contract.toml`:
  ```toml
  outcome = "…"
  protected_paths = ["Cargo.lock"]          # the contract itself is always protected, implicitly
  [budget]
  deadline = "2026-11-20T17:00:00Z"
  cost_ceiling_cents = 2000                 # optional
  [[script]]
  name = "tests"
  run = ["cargo", "test", "--workspace"]
  kind = "test"                             # "test" | "check"
  required_tests = ["harness::contract::check::zero_tests_executed_fails_the_script"]
  min_tests = 1                             # default 1 for kind = "test"
  allow_test_removal = false
  [[rubric]]                                # phase 1: recorded and shown, not scored (the judge is phase 2)
  id = "docs"
  criterion = "User-facing behaviour is documented"
  pass_mark = 7
  scale_max = 10
  ```
- Produces `Contract::parse(&str) -> Result<Contract>`, `Contract::to_toml(&self) -> String` (canonical: serialise the struct, never echo the raw input) and `Contract::hash(&self) -> ContractHash` (blake3 of `to_toml()`).
- Produces `freeze(git: &Git, task: &TaskId, c: &Contract) -> Result<ContractHash>`:
  1. `git hash-object -w --stdin` for the TOML
  2. `git mktree` for a tree with `contract.toml`
  3. `git commit-tree`
  4. `git update-ref refs/ohara/tasks/<id>/contract <commit>`

  Also produces `load_frozen(git, task) -> Result<(Contract, ContractHash)>`.
- Produces `check(c: &Contract, expected: &ContractHash, cwd: &Path, deadline: Deadline, logs: &Path) -> Result<CheckReport { scripts: Vec<ScriptResult>, satisfied: bool }>`. If `c.hash() != *expected`, it returns `Error::Contract("contract hash mismatch")`.
- `test_output::parse(output: &str) -> TestSummary { ran: Option<u32>, passed_names: BTreeSet<String> }` understands three formats:
  - **cargo:** the sum of the `N passed; M failed` lines; `test <name> ... ok` gives a passed name
  - **pytest:** `N passed`; with `-v`, `<path>::<name> PASSED`
  - **jest/vitest:** `Tests: N passed`

  When `ran` is `None` for a `kind = "test"` script, the script fails with "could not count tests".
- Produces `Git::new(cwd)` with `run(args: &[&str]) -> Result<String>` and helpers `head()`, `reset_hard(sha)`, `commit_all(msg) -> Result<String>`, `worktree_add(path, branch, base)` and `diff_names(from, to)`. `commit_all` adds tracked and untracked files with `git add -A`; it is safe inside the task's own worktree only. Document that in the doc comment.

- [ ] **Step 1: Write failing tests:**
  - `contract_hash_is_stable_across_reparse`
  - `freeze_then_load_round_trips` (temp repo)
  - `check_refuses_hash_mismatch`
  - `zero_tests_executed_fails_the_script` (fixture output `running 0 tests … 0 passed`)
  - `missing_required_test_fails_and_is_named`
  - `cargo_pytest_jest_counts_parse`
  - `unparseable_test_output_fails_test_script`
- [ ] **Step 2: Run** `cargo test -p ohara-harness contract`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit.** **Open PR 6.**

### Task 9: Guard: protected paths and test weakening

**Files:**
- Create: `crates/ohara-harness/src/guard.rs`, `crates/ohara-harness/src/guard/weakening.rs`
- Test: inline tests, plus `crates/ohara-harness/tests/guard.rs` (a temp git repo)

**Interfaces:**
- Consumes: `Git`, `Contract`.
- Produces `guard(git: &Git, c: &Contract, from: &str, to: &str) -> Result<GuardReport { violations: Vec<Violation> }>`, where:
  ```rust
  pub enum Violation {
      ProtectedPathChanged(PathBuf),
      TestFileDeleted(PathBuf),
      TestsIgnored { file: PathBuf, added: u32 },
      TestCountDropped { file: PathBuf, before: u32, after: u32 },
      AssertionsDropped { file: PathBuf, before: u32, after: u32 },
  }
  ```
- Protected paths are `c.protected_paths`, the implicit `contract.toml` and `.ohara/**`, matched with `globset`.
- Test files are matched with `globset` on `**/tests/**`, `**/*_test.*`, `**/*_tests.rs`, `**/test_*.py`, `**/*.test.*`, `**/*.spec.*` and `**/src/test/**`.
- **Token counts per test file**, comparing `from` and `to` with `git show <rev>:<path>`. Count tokens; don't parse.

  | Count | Tokens |
  |---|---|
  | tests | `#[test]`, `#[tokio::test]`, `def test_`, `it(`, `test(`, `@Test` |
  | ignore markers | `#[ignore`, `@pytest.mark.skip`, `.skip(`, `xit(`, `@Disabled` |
  | assertions | `assert`, `expect(`, `assertEquals` |

- **Contract opt-outs:** `allow_test_removal = true` on any script suppresses `TestFileDeleted`, `TestCountDropped` and `AssertionsDropped`. Nothing suppresses `ProtectedPathChanged` or `TestsIgnored`.

- [ ] **Step 1: Write failing tests:**
  - `edit_to_protected_path_is_rejected_and_reset`: guard reports the violation; the supervisor reset is tested in Task 13.
  - `contract_toml_is_implicitly_protected`
  - `deleting_test_file_is_violation`
  - `adding_ignore_attribute_is_violation`
  - `removing_assertions_is_violation`
  - `allow_test_removal_suppresses_deletion_only`
  - `adding_tests_is_clean`
- [ ] **Step 2: Run** `cargo test -p ohara-harness guard`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit.** **Open PR 7.**

### Task 10: Decisions: the `propose_decision` tool and validation

**Files:**
- Create:
  - `crates/ohara-mcp/src/tools/propose_decision.rs`
  - `crates/ohara-harness/src/decisions.rs`
- Modify:
  - `crates/ohara-mcp/src/tools/mod.rs` (register the tool only when `--harness-task <id>` is passed)
  - the `ohara-mcp` arg parsing (add `--harness-task`)
  - `crates/ohara-mcp/Cargo.toml` (depend on `ohara-storage`, which it already wires, for `SqliteTaskStore`)
- Test: `crates/ohara-mcp/tests/propose_decision.rs`, inline tests in `decisions.rs`

**Interfaces:**
- MCP tool `propose_decision`:
  - **Input** (schemars): `{ question, options: [string], chosen, runner_up?, assumptions: [{text, check?: [string]}], cost_if_wrong: "low"|"medium"|"high", parent_ids?: [string] }`.
  - **Behaviour:** writes a `DecisionRecord` with `state = Proposed`, `round = task.round`, an empty `evidence` and no `checkpoint_sha`.
  - **Returns** `{ decision_id }`.
  - **Validation errors:** `chosen` isn't in `options`; there are fewer than 2 options; the task isn't `Running`.
- `decisions::settle_round(store, task, round, checkpoint_sha, scripts: &[ScriptResult], runtime: &RuntimeName) -> Result<SettleReport { settled: u32, rejected: u32 }>`. For every `Proposed` decision of the round, the supervisor:
  - **rejects** it with a `rejected_reason` when an assumption `check` argv exits non-zero at the checkpoint, or when a `parent_id` doesn't exist
  - otherwise **settles** it, attaching `checkpoint_sha`, the script results and the runtime (evidence only the supervisor produced)
  - adds `parent_ids` edges to every earlier settled decision whose checkpoint diff touches files overlapping this round's diff (spec §10: over-link rather than under-link)
- `decisions::mcp_config_for(task: &TaskId, dir: &Path) -> Result<PathBuf>` writes an MCP config JSON that runs `ohara-mcp --harness-task <id>`. The adapters receive it as `AgentRequest.mcp_config`.

- [ ] **Step 1: Write failing tests:**
  - `propose_writes_proposed_row`
  - `chosen_not_in_options_is_tool_error`
  - `tool_absent_without_harness_flag`
  - `settle_attaches_supervisor_evidence`
  - `failing_assumption_check_rejects`
  - `overlapping_files_add_parent_edge`
- [ ] **Step 2: Run** `cargo test -p ohara-mcp propose_decision` and `cargo test -p ohara-harness decisions`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** both commands. **Expected:** PASS.
- [ ] **Step 5: Commit.** **Open PR 8.**

### Task 11: Workflows with starters, and contract drafting

**Files:**
- Create:
  - `crates/ohara-harness/src/workflow.rs`
  - `crates/ohara-harness/src/workflow/starters/{bug,feature,research}.toml` (via `include_str!`)
  - `crates/ohara-harness/src/draft.rs`
  - `crates/ohara-harness/src/prompts/contract.md`
- Test: inline

**Interfaces:**
- Produces `Workflow { name, description, contract_template: ContractTemplate, roles: Option<RoleMap>, default_budget: Duration, diff_budget_lines: u32 }`.
  - `ContractTemplate` holds the default `scripts`, `rubric` and `protected_paths`.
  - `Workflow::load(name) -> Result<Workflow>` reads `ohara_home()/workflows/<name>.toml`. On first use it seeds the three starters into that directory if they're missing.
  - `Workflow::list() -> Result<Vec<Workflow>>`.
- The starters encode the spec's §12a table:

  | Starter | Scripts and rubric | Diff budget | Default budget |
  |---|---|---|---|
  | **bug** | one `kind = "test"` script; rubric "a failing reproduction test exists in the first increment" | 200 lines | 60 m |
  | **feature** | test script, plus rubric "design matches existing patterns" and "user-facing behaviour documented" | 400 lines | 120 m |
  | **research** | no scripts; rubric "every claim cites a source" and "the question is answered" | — | 60 m |

- Produces `draft_contract(runner: &RoleRunner, store, task: &TaskRecord, wf: &Workflow, cwd: &Path) -> Result<Contract>`:
  1. Run `Role::Contract` with `prompts/contract.md`, filled with the outcome, the template TOML, and repo hints: the `Cargo.toml`, `package.json` or `pyproject.toml` present.
  2. The agent writes `contract.draft.toml` in a scratch directory under the task directory, never in the worktree.
  3. Parse and validate it.
  4. If parsing fails, fall back to the template with `outcome` filled in, and set `TaskRecord.detail` to "drafting fell back to template".

- [ ] **Step 1: Write failing tests:**
  - `starters_seed_into_empty_home` (temp `OHARA_HOME`)
  - `user_edit_to_starter_is_not_overwritten`
  - `draft_uses_agent_output` (`FakeRuntime` writes a valid draft)
  - `invalid_draft_falls_back_to_template_with_detail`
- [ ] **Step 2: Run** `cargo test -p ohara-harness workflow draft`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit** (red, then green).

### Task 12: `ohara task`: create, approve, list, show

**Files:**
- Create:
  - `crates/ohara-cli/src/commands/task.rs` (clap surface + dispatch)
  - `crates/ohara-cli/src/commands/task/approve.rs` (edit loop)
- Modify:
  - `crates/ohara-cli/src/main.rs` (add `Cmd::Task`)
  - `crates/ohara-cli/Cargo.toml` (add `ohara-harness`)
  - `docs-book/src/cli/task.md`, plus its `SUMMARY.md` entry
- Test: `crates/ohara-cli/tests/task_cli.rs` (uses `FakeRuntime` via `ohara-harness`'s `test-util` feature, behind an env-selected runtime kind `fake`, which is compiled only with `test-util`)

**Interfaces:**
- CLI surface:
  - `ohara task "<outcome>" (--for <dur> | --until <RFC3339|HH:MM>) [--workflow <name>=feature] [--repo <path>=.] [--detach]`
  - subcommands `approve <id> [--edit]`, `list`, `show <id>`, `stop <id>`, `resume <id>` and `review <id>` (the last three are wired up in Tasks 13 and 15)
  - clap: `args_conflicts_with_subcommands = true`
- **Create:**
  1. Resolve the deadline. `--for` counts from approval, so store the duration in `detail` until approval. `--until` is absolute (spec §6).
  2. Create a `TaskRecord` in state `Drafting` and the worktree `$OHARA_HOME/tasks/<id>/worktree` on branch `ohara/<id>`, from `HEAD` of `--repo`.
  3. Draft the contract (Task 11) and move to `AwaitingApproval`.
  4. Print the contract TOML and the sandbox warning: "Phase 1: agents run with your permissions; no sandbox."
  5. Without `--detach`, prompt "Approve? [y/e(dit)/n]", where `e` opens `$EDITOR` on a temp copy and re-validates it. With `--detach`, print `ohara task approve <id>` and exit 0.
- **Approve:** freeze (Task 8), store `contract_hash`, set the final `deadline`, move to `Running`, then hand off to the supervisor (Task 13). Until Task 13 lands, this stops after `Running`.
- **List and show** read from the `TaskStore`. A `Running` task whose heartbeat is older than 60 s is shown as `interrupted`.

- [ ] **Step 1: Write failing tests:**
  - `detach_leaves_task_awaiting_approval`
  - `approve_freezes_contract_and_sets_running`
  - `for_duration_counts_from_approval`
  - `until_in_past_is_error`
  - `list_shows_interrupted_for_stale_heartbeat`
- [ ] **Step 2: Run** `cargo test -p ohara-cli --test task_cli`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS. Run `mdbook build docs-book`. **Expected:** no warnings.
- [ ] **Step 5: Commit.** **Open PR 9.**

### Task 13: The supervisor loop

**Files:**
- Create:
  - `crates/ohara-harness/src/supervisor.rs` (state machine)
  - `crates/ohara-harness/src/supervisor/clock.rs` (deadline, cost ceiling, heartbeat task)
  - `crates/ohara-harness/src/prompts/builder.md`
- Modify: `crates/ohara-cli/src/commands/task.rs` (approve, `resume` and `stop` call into the supervisor)
- Test: `crates/ohara-harness/tests/supervisor.rs` (a temp git repo with a tiny cargo crate, and `FakeRuntime`)

**Interfaces:**
- Consumes: `TaskStore`, `RoleRunner`, `Contract`/`check`, `guard`, `decisions::settle_round`, `Git`.
- Produces the phases from spec §13 that phase 1 uses: `Phase { Build, Guard, Commit, CheckContract, HandBack }`. Drafting and approval happen in the CLI.
- Produces `Supervisor::new(store: Arc<dyn TaskStore>, runner: RoleRunner, git: Git)` and `async fn run(&self, id: &TaskId) -> Result<TaskState>`.
- **Round loop**, starting from the last checkpoint (`HEAD` of the task branch):
  1. **Build:** run `Role::Builder` with `prompts/builder.md`. The prompt holds:
     - the outcome
     - the contract, read-only
     - the last check report's failures (head and tail of each log, 40 lines each)
     - the settled decisions so far (one line each)
     - an instruction to call `propose_decision` for every choice that wasn't forced by the contract
  2. **Guard:** any violation resets hard to the checkpoint, rejects the round's proposals with reason `guard: <violation>`, and starts the next round.
  3. **Commit:** run `git commit_all("ohara: round <n>")` to make the checkpoint, then `settle_round`.
  4. **CheckContract:** if the check is satisfied, go to `HandBack` with `Succeeded`. Otherwise increment `round` and loop.
- **Clock:**
  - Every agent request gets `deadline = task.deadline`.
  - A background tokio task writes `heartbeat` every 10 s.
  - Once the deadline passes, or `cost_spent >= cost_ceiling` (counting only runtimes that report cost), go to `HandBack` with `TimedOut` and `detail = "deadline" | "cost ceiling"`.
- **Stop:** `ohara task stop <id>` sets the state to `Stopped`. The supervisor checks the state between phases and hands back.
- **Resume:**
  1. Reset hard to the last recorded checkpoint (the newest step's `head_sha` where the step `Passed`, or else `base_sha`).
  2. Mark any `Running` step `Aborted`.
  3. Continue the loop.
- The supervisor runs in the foreground of `ohara task approve`. A detached run is `ohara task resume <id> &`; daemon hosting is phase 1b.

- [ ] **Step 1: Write failing tests:**
  - `fixes_failing_test_and_succeeds`: the fixture crate has one failing test; `FakeRuntime` edits the source file so it passes. The task ends `Succeeded` after one round.
  - `deadline_kills_running_step_and_hands_back_last_checkpoint`
  - `cost_ceiling_stops_task`
  - `guard_violation_resets_and_continues`
  - `resume_after_crash_restarts_from_last_checkpoint`: drop the supervisor future mid-round, construct a new one, resume, and assert round 1 isn't re-run.
  - `stop_between_phases_hands_back`
  - `heartbeat_updates_while_running`
- [ ] **Step 2: Run** `cargo test -p ohara-harness --test supervisor`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit.** **Open PR 10.**

### Task 14: Hand-back: increments and stacked branches

**Files:**
- Create: `crates/ohara-harness/src/handback.rs`, `crates/ohara-harness/src/handback/increments.rs`
- Modify: `supervisor.rs` (the `HandBack` phase calls into this)
- Test: inline tests, plus `crates/ohara-harness/tests/handback.rs`

**Interfaces:**
- Produces `cut_increments(git, base: &str, rounds: &[(u32, String /*checkpoint sha*/)], diff_budget_lines: u32) -> Result<Vec<Increment { n: u32, from: String, to: String, rounds: Vec<u32>, lines: u32 }>>`.
  - It groups consecutive rounds greedily while the cumulative `git diff --numstat` stays within `diff_budget_lines`.
  - A single round over budget becomes its own increment, flagged `over_budget: true`.
- Produces `publish(git, task, increments, pr: PrMode) -> Result<Vec<Delivery { branch, pr_url: Option<String> }>>`:
  - Creates the branches `ohara/<id>/<n>` at each increment's `to`.
  - For `PrMode::Draft` with a GitHub remote, runs `gh pr create --draft --base <previous branch or default> --head ohara/<id>/<n>`. It pushes first (`git push -u origin <branch>`), and only if the user passed `--push` at task creation. Otherwise it creates local branches only and prints the commands.
  - Default: `PrMode::LocalOnly`. Pushing is an outward action and needs the explicit flag.

- [ ] **Step 1: Write failing tests:**
  - `cuts_by_diff_budget`
  - `oversized_round_is_own_increment_flagged`
  - `local_only_creates_branches_no_push`
  - `each_increment_stacks_on_previous`
- [ ] **Step 2: Run** `cargo test -p ohara-harness handback`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. **Expected:** PASS.
- [ ] **Step 5: Commit** (red, then green).

### Task 15: Summary, `review.html` and `ohara task review`

**Files:**
- Create:
  - `crates/ohara-harness/src/handback/summary.rs`
  - `crates/ohara-harness/src/handback/review_html.rs`
  - `crates/ohara-harness/src/handback/review.html.tmpl` (via `include_str!`; inline CSS and JS, no network)
  - `crates/ohara-cli/src/commands/task/review.rs`
- Test: inline tests, plus `crates/ohara-cli/tests/task_cli.rs` additions

**Interfaces:**
- Produces `summary(task, steps, decisions, check: &CheckReport, deliveries) -> String` (Markdown). It contains, in this order:
  1. outcome, state, time used / budget and cost used / ceiling
  2. the script results
  3. the rubric criteria marked "not scored in phase 1"
  4. the decision trail, sorted by `cost_if_wrong` (high first), then by how many later decisions list it in `parent_ids`, one line each
  5. rejected decisions with their reasons
  6. failovers
  7. what wasn't done, when `TimedOut`
- It is written to `$OHARA_HOME/tasks/<id>/summary.md` and used as the body of every draft PR.
- Produces `write_review_html(path, task, steps, decisions, check) -> Result<()>`: a self-contained page that embeds the data as JSON. Its CSS defines light and dark tokens.
- `ohara task review <id>`:
  - For each settled decision (most consequential first), prompt `[u]pheld / [o]verturned / [s]kip` and an optional note.
  - Write `human_verdict` and `verdict_note`.
  - With `--json <file>`, apply the verdicts non-interactively (the future app uses this).

- [ ] **Step 1: Write failing tests:**
  - `summary_orders_by_consequence`
  - `summary_marks_rubric_unscored`
  - `review_html_is_self_contained` (no `http` URLs in the output)
  - `review_json_applies_verdicts`
- [ ] **Step 2: Run** `cargo test -p ohara-harness summary review_html` and `cargo test -p ohara-cli --test task_cli review`. **Expected:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** both commands. **Expected:** PASS. Then do the end-to-end smoke test in `fixtures/tiny/repo` with a real `claude` runtime:

  ```
  ohara task "make the failing test pass" --for 15m --workflow bug
  ```

  Approve the contract. **Expected:** the task reaches `succeeded` or `timed_out`, with branches, `summary.md` and `review.html` present. Record the outcome in the PR description.
- [ ] **Step 5: Commit.** **Open PR 11.**

---

## Out of this plan (later phases)

These are deliberately out of scope here:
- POCs, the judge and the rubric scoring (phase 2)
- the slot pool and the srt sandbox (phase 2)
- reopen and replay (phase 3)
- non-code tasks (phase 4)
- memory and `ohara eval` (phase 5)
- the desktop app and the synapse projection (phase 1b, its own plan)
