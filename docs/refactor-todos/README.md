# Refactor Backlog

Per-task refactor notes captured at landing time. The code-quality reviewer
already gates each task; *this* directory is the lightweight long-tail backlog
for "would be nice eventually" items that the reviewer flagged as Minor or
out-of-scope, plus any patterns the refactor-todo subagent spots independently.

## Conventions

- One file per plan task: `task-NN-<short-slug>.md` (NN = plan task number, two digits).
- Each item:
  - **Severity:** Low / Medium / High (subjective; gating items go to code review, not here).
  - **Location:** `file:line` (or a range).
  - **What:** the smell or opportunity.
  - **Why:** why it's worth changing.
  - **Suggestion:** concrete proposed change (one-liner where possible).
  - **Effort:** XS / S / M / L (rough sizing).

## Workflow

1. After a plan task lands and the test-runner reports pass, a refactor-todo
   subagent reads the task's diff and any related files, then writes
   `docs/refactor-todos/task-NN-<slug>.md`.
2. The file lands in the same commit chain as the task itself (separate
   commit with message `Capture refactor backlog for Task N: <slug>`).
3. Periodically (e.g., before milestones), batch-resolve items by severity.

## Status

Items live here until they're addressed. To close an item, delete it from the
file with a one-line note in the commit message describing where the change
landed (commit SHA or PR).
