-- PR #138 review: provenance of the shared HEAD-symbol snapshot.
-- Until main has an indexed tip, any linked worktree may replace the
-- snapshot, so a worktree's own tip says nothing about which commit the
-- snapshot describes. One row per repo names the commit (sha) and the
-- worktree ('' is main) whose run last wrote the snapshot. It is written
-- in the same transaction as the `symbol` rows; a snapshot written
-- without provenance deletes the row. Existing indexes start without a
-- row, so the snapshot owner's next `ohara index` refreshes it once.
CREATE TABLE head_symbol_snapshot (
    repo_id      TEXT PRIMARY KEY,
    worktree_key TEXT NOT NULL,
    sha          TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
