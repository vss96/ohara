//! Provenance of the shared HEAD-symbol snapshot (migration V8): which
//! worktree's run last replaced it, at which commit. Key `''` is main.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

/// The `(worktree_key, sha)` that last replaced the snapshot, if recorded.
pub fn get(c: &Connection, repo_id: &str) -> Result<Option<(String, String)>> {
    let row = c
        .query_row(
            "SELECT worktree_key, sha FROM head_symbol_snapshot WHERE repo_id = ?1",
            params![repo_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row)
}

/// Record that `(key, sha)` wrote the snapshot. Run it in the snapshot's
/// own transaction so the symbols and their provenance move together.
pub fn set(c: &Connection, repo_id: &str, key: &str, sha: &str) -> Result<()> {
    c.execute(
        "INSERT INTO head_symbol_snapshot (repo_id, worktree_key, sha, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo_id)
         DO UPDATE SET worktree_key = excluded.worktree_key,
                       sha = excluded.sha,
                       updated_at = excluded.updated_at",
        params![repo_id, key, sha, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Forget the provenance: the snapshot was replaced by a writer that did
/// not say which commit it describes.
pub fn clear(c: &Connection, repo_id: &str) -> Result<()> {
    c.execute(
        "DELETE FROM head_symbol_snapshot WHERE repo_id = ?1",
        params![repo_id],
    )?;
    Ok(())
}
