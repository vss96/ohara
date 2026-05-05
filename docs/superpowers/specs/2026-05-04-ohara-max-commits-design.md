# ohara — `--max-commits` flag for bounded index passes

**Status:** SUPERSEDED by
`docs/superpowers/specs/2026-05-05-ohara-plan-and-ignore-design.md`
(and its companion specs B + D). Same problem class — making giant
repos viable — solved with a smarter answer (path-aware filtering +
chunk dedup + parallel pipeline) instead of a blunt commit cap. Kept
for historical context; do not plan from this RFC.

**Goal:** let `ohara index` cap a fresh pass to the most recent N commits
reachable from HEAD, so very large repos (Linux: ~1.44 M commits) can be
made queryable in minutes instead of weeks.

## Why this matters

Today `ohara index <repo>` walks every commit reachable from HEAD on a
cold pass. That model is fine up to mid-six-figure histories but breaks
on truly large repos:

```
$ ohara status /Users/vss/Documents/linux
last_indexed_commit: <none>
commits_behind_head: 1,444,500
```

At even an optimistic 50 commits/sec end-to-end, that's ~8 hours of
walltime before the first `find_pattern` query returns useful results;
realistically with embedding it's days. There is no shipped knob that
makes the tool usable on this class of repo. Users either:

1. Skip ohara on the repo entirely (current Linux experience), or
2. Hand-roll a shallow clone in a separate worktree, which discards
   blame history and breaks `explain_change`.

A bounded "index the recent slice" mode covers the common case
("what does ohara know about the last year of changes?") and keeps
blame intact within that slice.

## Shape

A new flag on `ohara index`:

```
--max-commits <N>    Cap the cold-pass walk at the N most recent
                     commits reachable from HEAD (newest-first).
                     Mutually exclusive with --incremental.
                     No effect on --rebuild without an N value.
```

### Walker change

`GitWalker::list_commits` today emits topological-reverse (oldest first).
For `--max-commits N` the indexer needs the *newest* N, so we add a sibling
method:

```rust
/// Walk the N most recent commits reachable from HEAD, newest-first,
/// then return them in topological-reverse order (oldest of the slice
/// first) so the rest of the indexer pipeline is unchanged.
pub fn list_recent_commits(&self, n: usize) -> Result<Vec<CommitMeta>>;
```

Implementation: `Repository::revwalk()` without `Sort::REVERSE`, push
HEAD, take N, reverse the resulting Vec. We *don't* try to do a
streaming walk — N is bounded by the user, and the existing
`list_commits` already materialises a Vec, so this matches the
existing contract.

### Storage change

Add `oldest_indexed_commit` alongside `last_indexed_commit` in `repo`:

```sql
ALTER TABLE repo ADD COLUMN oldest_indexed_commit TEXT;
```

Set on every successful pass to the oldest SHA we wrote. This is the
piece that makes a future "extend backwards" command tractable; for v1
it's purely informational, surfaced via `ohara status`.

### Indexer change

`Indexer::run` gains an optional cap. When set, it asks the commit
source for the bounded slice instead of the full since-watermark walk:

```rust
let commits = match cap {
    Some(n) => commit_source.list_recent_commits(n).await?,
    None    => commit_source.list_commits(since).await?,
};
```

The per-commit loop is otherwise unchanged — including the v0.6.3
`commit_exists` skip, which keeps mixed `--max-commits` + resume
runs idempotent.

### CLI / status surface

`ohara status` learns one new line:

```
oldest_indexed_commit: 1a2b3c4 (1,000 commits before HEAD)
```

The "before HEAD" hint is a `count_between` call on the live repo;
it's purely a UX signal, not stored.

## Constraints

- **`--max-commits` and `--incremental` are mutually exclusive.**
  `--incremental` is a no-op-when-up-to-date path; combining the two
  has no clean semantics ("cap an empty walk"). Clap-level
  `conflicts_with`.
- **`--max-commits` and `--rebuild` compose.** `ohara index --rebuild
  --yes --max-commits 5000` is the documented path for "I built a too-
  small index, redo it bigger".
- **Watermark semantics unchanged.** A bounded pass that reaches HEAD
  sets `last_indexed_commit = HEAD`, so subsequent `--incremental`
  runs work normally.
- **`commits_behind_head` keeps current meaning.** It reports
  `count_since(last_indexed_commit)`. A bounded pass that finished at
  HEAD shows `0`, even though earlier history is unindexed. The
  `oldest_indexed_commit` line is what tells the user the slice is
  bounded.
- **No partial-pass watermark trickery.** If a `--max-commits 1000`
  pass crashes after 600 commits, resume is the existing v0.5.1
  abort-resume path: walk again, the v0.6.3 skip-if-exists check
  swallows the 600 already-written rows. We do *not* try to make
  resume "remember" the original cap.

## Non-goals

- **Backfilling older history.** Extending the indexed window
  backwards (`--max-commits 5000` after a `--max-commits 1000` run)
  needs a real commit-set watermark, not a single SHA. Out of scope;
  documented as a follow-up RFC.
- **Date-based windows.** `--since 2025-01-01` is a separate UX and
  needs a different walker entry point (revwalk filtering by commit
  time, not count). Could land later; not coupled to this change.
- **Sparse / first-parent-only walks.** Same family as date windows —
  separate flag, separate spec.
- **Auto-cap on huge repos.** Tempting ("if `commits_behind_head >
  500_000`, suggest `--max-commits`") but heuristics-in-CLIs age
  badly. Surface the data, let the user pick the cap.

## Success criteria

- `ohara index --max-commits 1000 <linux-repo>` finishes in a
  reasonable window (target: < 30 min on the developer's M-series box,
  matches the existing `commit_batch` budget at 1000 commits) and
  produces a queryable index.
- `find_pattern` against that index returns results from the indexed
  slice and never from older history.
- `ohara status` shows both watermarks (`last_indexed_commit` and
  `oldest_indexed_commit`) with the "N commits before HEAD" hint.
- A unit test on `list_recent_commits` covers: N less than total, N
  equal to total, N greater than total (returns whatever exists,
  doesn't error), and a multi-branch repo (newest-first ordering is
  stable).
- An integration test in `crates/ohara-cli/tests/` runs
  `--max-commits 5` against a 20-commit fixture, asserts the indexed
  set is exactly the newest 5, and that a follow-up `--incremental`
  is a no-op.
- A regression test that `--max-commits` + `--incremental` is a clap
  error.

## Out of scope (deferred)

- A full commit-set watermark (interval tree of indexed commits) that
  would unlock both backfill and force-push handling. Tracked
  separately.
- A `--rebuild-shallow` ergonomic alias for `--rebuild --yes
  --max-commits N`. Wait for usage data before adding sugar.
- MCP-side surfacing of the bounded-index state (e.g. find_pattern
  noting "results limited to the last N commits"). Useful but
  separate; depends on this landing first.

## Related

- `crates/ohara-git/src/walker.rs` — gains `list_recent_commits`.
- `crates/ohara-git/src/lib.rs` — `GitCommitSource` plumbs the new
  method through the `CommitSource` trait.
- `crates/ohara-core/src/indexer.rs` — branches on the optional cap.
- `crates/ohara-storage/src/tables/repo.rs` — schema migration +
  read/write of `oldest_indexed_commit`.
- `crates/ohara-cli/src/commands/index.rs` — `--max-commits` clap
  arg, `conflicts_with = ["incremental"]`.
- `crates/ohara-cli/src/commands/status.rs` — render the new line.
- `docs-book/src/architecture/indexing.md` — short paragraph on the
  bounded-pass mode and what's *not* covered (backfill).
- Prior art: `2026-05-02-ohara-v0.6.3-resume-skip-rfc.md` (the
  `commit_exists` skip this design relies on).
