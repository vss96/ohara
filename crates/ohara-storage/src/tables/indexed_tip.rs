//! Per-worktree indexed tips (plan 33). Key `''` is the main worktree.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

/// All `(worktree_key, sha)` rows for a repo, ordered by `worktree_key`.
pub fn list(c: &Connection, repo_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare(
        "SELECT worktree_key, sha FROM indexed_tip WHERE repo_id = ?1 ORDER BY worktree_key",
    )?;
    let rows = stmt.query_map(params![repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Insert or replace the tip for `(repo_id, key)`; stamps `updated_at` now.
pub fn upsert(c: &Connection, repo_id: &str, key: &str, sha: &str) -> Result<()> {
    c.execute(
        "INSERT INTO indexed_tip (repo_id, worktree_key, sha, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo_id, worktree_key)
         DO UPDATE SET sha = excluded.sha, updated_at = excluded.updated_at",
        params![repo_id, key, sha, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Delete the tip for `(repo_id, key)` only while it names `sha`.
/// Returns whether a row was deleted.
pub fn remove_if(c: &Connection, repo_id: &str, key: &str, sha: &str) -> Result<bool> {
    let n = c.execute(
        "DELETE FROM indexed_tip WHERE repo_id = ?1 AND worktree_key = ?2 AND sha = ?3",
        params![repo_id, key, sha],
    )?;
    Ok(n > 0)
}

/// Move the tip for `(repo_id, key)` from `expected` to `new` only while it
/// names `expected`; stamps `updated_at`. Returns whether it moved.
pub fn replace_if(
    c: &Connection,
    repo_id: &str,
    key: &str,
    expected: &str,
    new: &str,
) -> Result<bool> {
    let n = c.execute(
        "UPDATE indexed_tip SET sha = ?4, updated_at = ?5
         WHERE repo_id = ?1 AND worktree_key = ?2 AND sha = ?3",
        params![repo_id, key, expected, new, Utc::now().to_rfc3339()],
    )?;
    Ok(n > 0)
}
