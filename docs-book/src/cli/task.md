# `ohara task`

Hand an outcome to agents, bounded by a time budget and a contract you
approve first. Phase 1 covers creating, approving, listing and showing
tasks.

> **Phase 1: agents run with your permissions; no sandbox.** They work in
> a private git worktree, never in your checkout, but they are not
> confined to it.

## Usage

```
ohara task "<outcome>" (--for <DURATION> | --until <TIME>) [--workflow <NAME>] [--repo <PATH>] [--detach]
ohara task approve <ID> [--edit]
ohara task list [--json]
ohara task show <ID> [--json]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--for` | | Time budget counted from approval: `90m`, `2h`, `1h30m` (units `s`, `m`, `h`, `d`). |
| `--until` | | Absolute deadline: an RFC 3339 timestamp, or `HH:MM` today in local time. A time in the past is an error. |
| `--workflow` | `feature` | Workflow template (`$OHARA_HOME/workflows/<name>.toml`; starters `feature`, `bug`, `research` are seeded on first use). |
| `--repo` | `.` | Repository to work on. |
| `--detach` | off | Draft the contract and exit; approve later with `ohara task approve <ID>`. |

## What creating a task does

1. Records the task and creates a worktree at
   `$OHARA_HOME/tasks/<ID>/worktree` on branch `ohara/<id>`, starting at
   the repo's current `HEAD`.
2. Has the configured contract agent adapt the workflow's template to the
   repo, inside that worktree. If the draft is unusable, the template is
   used and the task's detail says so.
3. Prints the contract (TOML) and waits for `Approve? [y/e(dit)/n]`:
   `y` approves, `e` opens `$EDITOR` and re-validates on save, `n` leaves
   the task awaiting approval.

## Approving

`ohara task approve <ID>` freezes the contract under
`refs/ohara/tasks/<ID>/contract`, stores its hash, sets the final
deadline (`--for` counts from this moment), and moves the task to
`running`. The contract's own `budget.deadline` is always set to the
task's final deadline. A contract with no scripts is refused unless the
workflow is `research`: with nothing to run, nothing could verify the
work. Supply scripts with `--edit`.

`approve` (without `--edit`) refuses a contract that differs from the one
that was shown at creation; use `--edit` to review it. It prints the
frozen contract, its hash and the sandbox warning. If a previous approve
froze the contract but could not update the task, re-running it adopts
that freeze.

If creation fails after the worktree exists, the task is marked
`aborted` and its worktree and branch are removed. `ohara task` also
refuses to run when `$OHARA_HOME` is inside the repository, and warns if
the checkout has uncommitted changes (they are not part of the task's
starting point).

## List and show

`list` and `show` read the task log. A `running` task whose heartbeat is
older than 60 seconds is shown as `interrupted`. Until the supervisor
lands (next PR), no process runs approved tasks, so they show
`interrupted` a minute after approval.
