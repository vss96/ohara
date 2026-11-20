# `ohara prune`

Remove index rows for commits that no branch, tag or worktree `HEAD`
reaches any more, and tidy the per-worktree indexed tips. Run it after
deleting branches, removing worktrees, or rewriting history (reset,
amend, rebase) to reclaim space and to let retrieval skip the
visibility filter again.

## Usage

```
ohara prune [PATH]
```

| Flag | Default | Description |
|------|---------|-------------|
| `PATH` (positional) | `.` | Path to the repo or any of its linked worktrees (they share one index). |
| `--no-daemon` (global) | off | Prune in this process even when a daemon is running. |

## What it removes

- **Commits** that no ref (branches, tags, remotes) and no worktree
  `HEAD` (detached ones included) reaches, with their hunks, vectors
  and full-text rows.
- **Retired tips** that nothing reaches any more. A worktree that moves
  to a commit that does not descend from its indexed tip (reset, amend,
  rebase, a reused slot) keeps the old tip as a *retired tip* so its
  commits stay behind the visibility filter. While any retired tip
  exists that the querying `HEAD` can't reach, every query pays for the
  filter; pruning drops the unreachable ones.
- **Tips of removed worktrees.** A linked worktree that no longer
  exists has its tip retired while a ref still reaches it, and dropped
  otherwise. The main checkout's tip is never removed.

## Where it runs

Like `ohara index --incremental`, `prune` hands the work to a running
`ohara serve` daemon of the same version when there is one, so the
daemon stays the single writer for the index. It prunes in-process
only when no such daemon is running or the daemon can't be reached; if
the daemon accepted the request and then failed or timed out, the
command fails instead of pruning in-process. The in-process path needs
only the index and git, so it never loads the embedding or rerank
model. `prune` never starts a daemon.

You rarely need to run it by hand when a daemon is running: after each
successful index pass, the daemon prunes on its own, but only when some
retired tip has become unreachable. A no-op request (the commit was
already covered) runs no pass and does not prune.

A retired tip that a branch still reaches is never pruned: it keeps the
visibility filter on for every `HEAD` that can't reach it until that
branch is deleted. Delete the branch, then run `ohara prune` (or let
the daemon's next index pass notice).

## Example

```sh
ohara prune
```

Sample output:

```
removed 3 unreachable commit(s) and 1 tip(s)
```

See [Worktrees and the daemon writer](../architecture/indexing.md#worktrees-and-the-daemon-writer)
for how tips, retired tips and the visibility filter fit together.
