# ohara outcome harness: design

Date: 2026-11-20
Status: draft for approval
Supersedes: the task model, blueprints (§6), verification (§10) and memory
(§8) of `2026-11-20-ohara-harness-design.md`. That spec lands with PR #138
on the `feat/shared-worktree-store` branch. Its agent runtime (§4),
isolation (§5), shared index store (§3), run log and resume, and security
(§11) still apply, unless something below changes them.

## 1. Intent

The harness lets a person hand an LLM a task and trust the result without
watching it. A task is an **outcome** plus a **time limit**. The harness
works toward the outcome until the outcome is verified or the time runs out:

- Scripts verify what can be checked mechanically.
- An LLM judge verifies the rest against a rubric.
- Every decision taken without a human is written down with its evidence,
  so a human can do the final review quickly.
- When something learned later proves an earlier decision wrong, the
  harness corrects it, and a redo is cheap because of what it recorded.

Three things make each run better than a bare agent loop:
- **The ohara index** answers "how was this done before" and "why does
  this look like this".
- **Memory** carries lessons from earlier runs and from the human's
  verdicts.
- **Saved workflows** reuse task shapes that worked before.

The harness is generic. A coding task in a repo is one kind of task.
Research, writing and data work are others.

### Why

Today an LLM declares its own work done, and a person has to check it.
The harness should replace "trust me" with three things: checks the person
agreed to before the work started, evidence for every choice, and an
explicit record of what was decided without them.

## 2. Goals, non-goals, anti-goals

**Goals**
- `ohara task "<outcome>" --until <time>`. The harness drafts the checks
  and the rubric. The person approves or edits them, then they are frozen
  and the clock starts. No further human input is needed until hand-back.
- Decisions are explored breadth first. When a decision is uncertain, the
  harness builds proof-of-concept builds (POCs) side by side, and a judge
  picks among them.
- Every unsupervised decision is recorded with its options, evidence,
  choice, assumptions and cost-if-wrong.
- Self-correction: wrong decisions found later are reopened. The harness
  rewinds, then replays later decisions fast from their records.
- Hand-back at the deadline or on success:
  - the best state reached
  - check results and judge scores
  - the decision trail, ordered by consequence
- Work is delivered as small, stacked increments (one per settled decision
  group), never as one large change. This is a core harness principle.

**Non-goals (for now)**
- Multi-user or hosted service, web UI, chat triggers.
- Our own LLM client or agent loop. Headless CLIs (`claude`, `codex`,
  `agy`, or any CLI through the generic adapter) stay behind the trait
  (§8a).
- Auto-merge or auto-publish. Delivery is always a draft for a human.
- Tasks spanning several unrelated repos.

**Anti-goals: these make it a failure even if a run "succeeds"**
- The run reaches green by changing its own checks, rubric or tests.
- A decision shows up in the result that is missing from the record.
- The run burns the whole time budget re-exploring the same decision.
- The hand-back is a wall of text that a human cannot review faster than
  they could redo the work.

## 3. Approaches considered

| | Approach | Trade-off | Outcome |
|---|---|---|---|
| 1 | **Supervisor-enforced.** A Rust supervisor (`ohara-harness`) owns everything the model must not bend: the clock, frozen checks, running scripts, checkpoints and rewind, the decision log, and judge isolation. Agents do the creative work. | More Rust, and changing the enforcement logic needs a rebuild. In return, the guarantees actually hold. | **Chosen by the user.** |
| 2 | **Agent-driven.** The loop lives in a skill or prompt, and ohara-mcp only offers tools such as `record_decision`, `checkpoint` and `judge`. | Fastest to build and change. Nothing is guaranteed, because a model can skip a step. Plan-33's execution shows that happens. | Rejected. |
| 3 | **Minimal.** Keep the fixed blueprint from the earlier spec and add a time budget and a decision log. | Smallest change, but it delivers none of breadth-first exploration, POCs, the judge, or self-correction. | Rejected. |

The chosen design keeps option 2's agility where it is harmless. The
order of supervisor phases is data (§9), so the shape of the loop can be
tuned without touching the parts that enforce the rules.

## 4. Task model

```
Task {
  id: TaskId (ULID)
  outcome: String                  // the person's words, verbatim
  deadline: Instant                // --until 17:00 | --for 90m
  cost_ceiling: Option<UsdCents>   // second stop condition (default from config)
  workspace: Workspace             // a git worktree; see §5
  contract: Contract               // frozen before the clock starts; see §6
  state: drafting | awaiting_approval | running | succeeded | timed_out | aborted
}
```

- A task always has an outcome and a deadline. Cost is a second ceiling,
  because a long loop on an expensive model can cost money before it runs
  out of time.
- Stop conditions, whichever comes first:
  1. the contract is satisfied (`succeeded`)
  2. the deadline passes (`timed_out`)
  3. the cost ceiling is reached (`timed_out`, with the reason recorded)
  4. an unrecoverable error occurs (`aborted`)
- After `succeeded`, the run stops. Remaining time is not spent polishing.
  Improvement happens at decision points (§7), not after the outcome is
  met.

## 5. Workspace: every task is a git repo

- A **code task** runs in a worktree of the target repo, using the slots
  and srt sandbox from the earlier spec (§5 there).
- A **non-code task** (research, writing, data) runs in a fresh repo under
  `~/.ohara/tasks/<id>/`, and every artifact lives there.

Because every task is in git:
- A **checkpoint** is a commit.
- A **rewind** is a reset to a checkpoint commit.
- A **POC** is a sibling worktree. With the shared store from plan-33,
  sibling worktrees share one index for free.
- The **ohara index** covers the task's own history. "Why did we pick
  this source" is answerable the same way as "why did we change this
  function".
- **Delivery** is a set of stacked branches, PRs or bundles, one per
  increment.

## 6. The contract: turning an outcome into checks

Drafting and freezing:
1. The harness drafts a contract from the outcome. It uses saved workflows
   (§11), memory, and for code tasks the ohara index.
2. It shows the person:
   - **scripts**: commands with expected results (`cargo test -p x`, `test
     -f report.md`, `python check_totals.py`)
   - **rubric**: criteria with a scale and a pass mark, for what scripts
     can't check (for example "every claim cites a primary source", or
     "the migration guide covers rollback")
   - **budget**: deadline and cost ceiling
   - **protected paths**: files the run may not modify, which always
     include the contract itself
3. The person approves or edits the contract, typically in under a minute.
   This is the only human decision before hand-back.
4. The approved contract is frozen and hashed.
   - It is stored on a side ref (`refs/ohara/tasks/<id>/contract`), with
     its hash in the run log. It never appears in delivered branches or
     PRs.
   - The supervisor refuses any later check run whose contract hash
     differs.
   - Any agent edit to a protected path is rejected at the `guard` step
     (earlier spec §6).

**Approval flow.**
- Drafting ends in `awaiting_approval`. `ohara task` shows the draft and
  waits in the foreground by default. With `--detach` it returns at once,
  and `ohara task approve <id> [--edit]` starts the run later.
- The clock starts at approval:
  - `--for 90m` counts from approval.
  - `--until 17:00` is absolute, so time spent waiting for approval comes
    out of the budget.
- Eval replays use contracts that are already frozen in the case file and
  skip approval. Contract drafting is measured separately, by comparing
  the drafts against those frozen contracts.

**Guarding against weakened tests.** Code tasks must edit test files, so a
path guard is not enough. Contract scripts may name **required tests**:
tests that must exist, must run, and must pass. The `guard` step also
flags, and rejects unless the contract allows it:
- deleted tests
- tests newly marked `#[ignore]` or skipped
- test files whose assertion count dropped
- any script that passes with zero tests executed

All of this is in phase 1.

"Satisfied" means every script passes and every rubric criterion meets its
pass mark. The judge (§8) scores the rubric.

## 7. The decision loop

The run is a sequence of **decision rounds**. Each round handles a
breadth-first frontier:

1. **Plan the frontier.** A planning agent lists the open decisions that
   the next increment depends on. Each one has two to four options and a
   short note on which are uncertain. Decisions that are certain are
   recorded and applied at once.
2. **Explore an uncertain decision.**
   - The supervisor writes the decision's rubric and freezes it *before*
     any POC exists. These rubric criteria are local to the decision,
     separate from the task contract.
   - It then builds one POC per surviving option, in parallel sibling
     worktrees, up to `max_parallel_pocs`.
   - Each POC gets a time slice (§10).
   - A POC must pass the task's scripts that apply to it, which is the
     floor, before the judge sees it.
   - **A POC is real work** (decided by the user). It is built to
     production quality, and the winner merges into the main line as-is.
     There is one exception (builder's call): the planner may mark a
     decision `sketch_only` when it is purely exploratory, for example
     "which library is faster". The POCs are then cheap spikes, and once
     the judge picks, the chosen option is built properly on the main
     line.
   - **Who builds what.** Builder sessions build POCs. Certain decisions
     are applied by one builder session per round, which works through the
     certain decisions in order.
   - **One uncertain decision is explored at a time.** Its POCs run in
     parallel, and every POC branches from the current main line. The
     other open decisions in the frontier wait.
     `max_parallel_pocs` is global and bounded by the slot pool. A POC
     uses a slot from the earlier spec §5, with warm caches, never a cold
     ad-hoc worktree.
   - **Rubric author.** The per-decision rubric is drafted by a dedicated
     session. That session is neither the planner that proposed the
     options nor any builder.
3. **Judge.** The judge (§8) scores the surviving POCs against the frozen
   rubric. The best one wins. The runner-up is kept as a branch, not
   deleted.
4. **Commit.** The winner is merged into the main line, the record (§9) is
   written, and a checkpoint commit is made. The losing worktrees are
   removed, apart from the runner-up's branch.
5. **Check the contract.** All scripts and the judge's contract-rubric
   pass run against the main line.
   - If the contract is satisfied, the run moves to hand-back.
   - Otherwise the next round plans a new frontier from the remaining gap.
     The gap is what failed and why, using the head-and-tail failure
     output format from the earlier spec §10.

The loop stops when the contract is satisfied or the clock runs out. There
is no fixed round count.

## 8. The judge

- A **separate session** in the agent runtime, which may use a different
  model from the builder (`[judge] model` in config). It never shares a
  session with an agent that built a POC.
- **Blind.** POCs are presented in shuffled order, as diffs or artifacts
  plus their script results, with author notes stripped.
- **Frozen rubric.** It scores against criteria written before the POCs
  existed. It returns per-criterion scores and a short rationale.
- **Recorded.** The rubric, scores, rationale and script results all go
  into the decision record.
- A judge failure (timeout, malformed output) is retried once with a fresh
  session. If it fails again, the supervisor takes the option with the best
  script results and records "judge unavailable".

### 8a. Agent runtimes: any CLI, alone or mixed (decided by the user: set in config)

Every agent role runs on a headless CLI behind the `AgentRuntime` trait
from the earlier spec §4. The roles are planner, rubric author, builder,
judge, reviewer and contract drafter.

- **First-class adapters: `claude`, `codex` and `agy`.**
  - Each adapter maps a step onto the CLI's flags: prompt, tool policy,
    turn and time limits, working directory.
  - Each one parses the CLI's event stream for completion and for cost,
    where the CLI reports it.
  - Known quirks belong inside the adapter. Two for `agy`: the prompt must
    be passed as `-p="<text>"` or it swallows the following flag, and
    headless mode auto-denies shell permissions, so the adapter must grant
    them explicitly.
- **A generic adapter, defined in config, for any other CLI.** The config
  gives:
  - a command template
  - how the prompt is passed (`arg` or `stdin`)
  - how "done" is detected (exit code, optionally plus an output pattern)

  Its guarantees are weaker, and the hand-back states that it was used:
  - no tool subsetting (protected-path and test-weakening checks still run
    in `guard`, outside the agent)
  - no cost reporting (cost is then unknown, and the cost ceiling only
    counts runtimes that report cost)
- **Mixing is set in config.** The harness does not choose runtimes on its
  own.

```toml
[runtimes.claude]  kind = "claude-code"
[runtimes.codex]   kind = "codex"
[runtimes.agy]     kind = "agy"        # model = "..." optional
[runtimes.local]   kind = "generic"
  command = ["my-agent", "--yes", "{prompt}"]
  prompt = "arg"
  done = { exit_code = 0 }

[roles]
planner  = ["claude", "codex"]     # first is primary; the rest are the failover order
builder  = ["codex", "claude"]
judge    = ["agy", "claude"]       # prefer a different model family from the builder
reviewer = ["claude"]
contract = ["claude"]

[poc]
builders = ["codex", "claude"]     # optional: POCs of one decision rotate across these
```

- **Single runtime.** Every role lists the same runtime, so one CLI runs
  the task end to end.
- **Failover.** A quota, rate-limit, auth or crash failure moves the step
  to the next runtime in the role's list. The step restarts from its
  checkpoint. All run state lives in `harness.sqlite` and git, never in a
  CLI session, so a switch loses only that step's turn in progress.
  Failovers are recorded.
- **Recorded.** Every decision record names the runtime and model that
  built each POC, and the one that judged it. Over time, memory can show
  which runtime does better on which kind of work. Letting the harness
  pick runtimes from that evidence is a possible later addition. It is not
  in this design.

## 9. Decision records, checkpoints, assumptions

Records are stored in `harness.sqlite` (earlier spec's run log), one row
per decision:

```
decision {
  id, task_id, round, parent_ids[],          // which decisions this one built on
  question, options[], chosen, runner_up,
  evidence: { pocs[], script_results[], judge: {rubric, scores, rationale} | null },
  assumptions: [ { text, check: Option<Command> } ],
  cost_if_wrong, checkpoint_sha,
  state: settled | reopened | superseded,
  superseded_by, reopen_reason,
  human_verdict: null | upheld | overturned, verdict_note
}
```

- **Assumptions are checks where possible.** "The tip list is consistent
  with the rows read" becomes a command or test. Free-text assumptions are
  allowed but marked unchecked.
- **`parent_ids`** links a decision to the earlier decisions it relied on.
  This is what makes replay (§10) targeted rather than total.
- Agents propose records through an ohara-mcp tool (`propose_decision`).
  The supervisor validates the fields, attaches evidence it produced
  itself (scripts and judge), and assigns the checkpoint. An agent cannot
  write a record whose evidence the supervisor did not produce.

## 10. Self-correction

A decision is reopened **retroactively**, when something learned later
proves it wrong. There are three triggers:
1. **Downstream failure.** A later script failure is traced, by blame on
   the failing lines plus the decision's `checkpoint_sha`, to an earlier
   decision.
2. **Broken assumption.** The supervisor re-runs every settled decision's
   assumption checks after each round. A failing check reopens that
   decision.
3. **Independent review.** At milestones (every N rounds, and before
   hand-back), a review agent reads the decision records and the diff
   since the last milestone. It never wrote any of them. It may file a
   reopen request with a reason.

Redo (rewind and fast replay):
1. Reset the main line to the reopened decision's checkpoint.
2. Try the runner-up first. It was already built and judged, so this costs
   only a re-merge and a contract check. If it is no longer viable, explore
   the decision again (§7).
3. Replay forward: for each later decision, in order:
   - If the decision does not depend on the reopened one (not reachable
     through `parent_ids`) and its assumption checks pass, re-apply its
     recorded choice. No POCs, no judge. Re-applying is mechanical: the
     decision's recorded commits are cherry-picked onto the rewound main
     line.
   - Otherwise reopen it as well. A cherry-pick conflict, or a script
     that fails after the cherry-pick, also counts as a reopen.
   - **Dependency edges are not left to the agent alone.** The supervisor
     adds an edge between any two decisions whose commits touch
     overlapping files. Over-linking costs replay time, while under-linking
     would silently keep code built on a reversed decision, so it errs
     toward linking.
   - **Rubric on reopen.** A reopened decision explored again gets a fresh
     rubric that includes the `reopen_reason`. The old rubric stays in the
     superseded record.
4. Record the reversal: the old record becomes `superseded` with
   `reopen_reason`, linked to the new one.

Thrash guard:
- A decision may be reopened at most `max_reopens` times (default 2).
  After that it is frozen as-is and flagged at the top of the hand-back.
- Reopens draw from the same clock.

## 11. Memory, workflows and the ohara index

**Memory** (`memory` table from the earlier spec §8). It becomes central
and is no longer a phase-5 optional extra. Sources:
- **Human verdicts.** An overturned decision becomes a lesson scoped to the
  paths or topic, for example "don't fall back silently after the daemon
  accepted the request". An upheld decision reinforces one.
- **Reversals within runs.** A decision reopened for the same reason
  across several runs becomes a lesson.

Memory is recalled when drafting the contract (§6) and when planning
frontiers (§7). A run must still work with memory switched off, and
memory's effect is measured with `ohara eval` (earlier spec §10).

**Workflows** are saved task shapes: a contract template, typical
frontiers and default rubrics for a kind of task ("add a CLI subcommand",
"literature review", "schema migration"). A workflow is saved from a
successful run whose decisions were upheld. It is suggested when drafting
the contract for a similar outcome. Workflows are data, not code.

**The ohara index** is used at three points:
- **Contract drafting.** It suggests scripts from how similar past changes
  were verified, and it suggests the increment size from how past changes
  in the same paths were sized and split.
- **Frontier planning.** `find_pattern` finds prior art; `explain_change`
  explains why existing code looks the way it does.
- **Self-correction.** Blame finds which decision introduced the lines a
  failure points at.

## 12. Hand-back

**What an increment is.** An increment is the main-line commits of one or
more consecutive decision rounds, cut by the planner under a diff budget.
The diff budget comes from the contract, and its default comes from the
ohara index (§11): how past changes in those paths were sized. Each
increment passes the contract's scripts on its own, and stacks on the
previous increment.

The hand-back is a draft for each increment: a stacked PR for code, or a
bundle in the task repo otherwise. It includes a summary:
- outcome, state, time and cost used
- the contract results: every script and every rubric criterion with its
  score
- the decision trail, **most consequential first**:
  - consequence is ordered by `cost_if_wrong`, then by the number of later
    decisions that depend on it
  - each entry is one line, with a link to the full record, POCs and
    judge rationale
- reopened decisions and why, and anything frozen by the thrash guard
- what was not done, if the run timed out

`ohara task review <id>` lets the person mark each decision upheld or
overturned, with a note. Those verdicts feed memory (§11).

## 13. Supervisor phases as data

```rust
pub enum Phase {
    DraftContract, AwaitApproval, PlanFrontier, Explore, Judge,
    Commit, CheckContract, Review, Reopen, HandBack,
}
```

The supervisor runs a fixed state machine over these phases. Which phases
run, and their limits, come from config:
- `max_parallel_pocs`
- `max_reopens`
- `review_every_rounds`
- the judge model
- time-slice policy

The phases themselves are code, and they are where the enforcement lives.
Their order and parameters are data.

## 14. Time slicing

- The remaining time is shared across the open frontier. Each uncertain
  decision gets `remaining / (open_decisions + reserve)`, where `reserve`
  is a share (default 20%) held back for contract checks, reviews and
  reopens.
- A POC that overruns its slice is stopped and judged as it stands, with
  "incomplete" marked.
- Time a decision doesn't use returns to the pool.

## 15. Phasing

Nothing from the earlier spec's harness exists in code yet. Only plan-33's
shared index store is built. Decided by the user:
- `ohara task` replaces the earlier spec's planned `ohara run`. There is
  one command, and a code change is one kind of task.
- The earlier spec's phase plan is replaced by the one below. Its pieces
  are pulled in only where a phase needs them.

1. **Contract and clock.**
   - Builds the minimal `ohara-harness` crate: supervisor state machine,
     the agent runtime trait with `claude`, `codex`, `agy` and generic
     adapters, the `[roles]` config with failover (§8a), and
     `harness.sqlite` with the run log, decision records and crash resume.
     Each adapter is its own small PR in the stack.
   - Covers the task model, contract draft, approve and freeze (with the
     test-weakening guard), one linear builder pass per round, a contract
     check loop until the deadline, the agent-proposed and
     supervisor-validated decision log, and hand-back.
   - Code tasks only, in **plain git worktrees with no srt sandbox**. The
     agent runs with the user's normal permissions. The user accepted this
     trade-off for phase 1.
   - Needs the CONTRIBUTING §9 amendment for a second SQLite file
     (earlier spec open question 4).
2. **POCs, judge and isolation.**
   - Brings in the earlier spec's warm slot pool and srt sandbox, since
     parallel POCs need several slots anyway.
   - Adds uncertain decisions, the frozen per-decision rubric, the blind
     judge, and `sketch_only` decisions.
3. **Self-correction.** Assumption checks, downstream tracing, milestone
   review, rewind and fast replay, thrash guard.
4. **Generic tasks.** Non-code task repos under `~/.ohara/tasks/`, and
   non-PR delivery bundles.
5. **Memory, workflows and evals.** Verdict capture, lessons and saved
   workflows. `ohara eval` comes from the earlier spec §10, with frozen
   contracts in case files, and is used to measure memory and to tune
   defaults.

Each phase ships as its own small stacked PRs.

## 16. Left to the builder

- The exact prompt templates for the planner, the POC builder, the judge
  and the reviewer.
- The `harness.sqlite` table layout beyond the `decision` fields above.
- How blame-based tracing maps a failure to a decision when a line was
  touched by several decisions. Take the latest as a first cut.
- The format of non-code delivery bundles.
- Default values (pass marks, slice reserve, `max_parallel_pocs`), to be
  tuned with `ohara eval`.
