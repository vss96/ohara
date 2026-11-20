-- Plan 33: one indexed tip per worktree replaces the single
-- repo.last_indexed_commit watermark. '' is the main worktree.
-- repo.last_indexed_commit is kept but no longer read.
CREATE TABLE indexed_tip (
    repo_id      TEXT NOT NULL,
    worktree_key TEXT NOT NULL,
    sha          TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (repo_id, worktree_key)
);
INSERT INTO indexed_tip (repo_id, worktree_key, sha, updated_at)
    SELECT id, '', last_indexed_commit, COALESCE(indexed_at, '')
    FROM repo WHERE last_indexed_commit IS NOT NULL;
