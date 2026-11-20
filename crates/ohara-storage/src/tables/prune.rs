//! Deleting commits that are no longer reachable from any ref or worktree
//! HEAD (plan 33).
//!
//! Content-addressed caches (`blob_cache`, `chunk_embed_cache`), the shared
//! `file_path` dictionary and the HEAD-symbol snapshot (`symbol`,
//! `vec_symbol`, `fts_symbol*`) are not keyed by commit and are kept.

use anyhow::Result;
use rusqlite::{params, Connection};

/// Every `commit_record.sha` in the index, in no particular order.
pub fn list_commit_shas(c: &Connection) -> Result<Vec<String>> {
    let mut stmt = c.prepare("SELECT sha FROM commit_record")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Statements run per sha, in order. Hunk mirrors go before `hunk` (their
/// sub-select needs the hunk rows); `hunk` goes before `commit_record`
/// (foreign key); `hunk_symbol` rows go with `hunk` via `ON DELETE CASCADE`.
const DELETE_PER_COMMIT: [&str; 6] = [
    "DELETE FROM fts_hunk_text WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha = ?1)",
    "DELETE FROM fts_hunk_semantic WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha = ?1)",
    "DELETE FROM vec_hunk WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha = ?1)",
    "DELETE FROM hunk WHERE commit_sha = ?1",
    "DELETE FROM vec_commit WHERE commit_sha = ?1",
    "DELETE FROM fts_commit WHERE sha = ?1",
];

/// One transaction per call: vec_commit, fts_commit, the hunks' fts_hunk_text /
/// fts_hunk_semantic / vec_hunk rows, hunk (hunk_symbol cascades), commit_record.
///
/// Returns the number of `commit_record` rows deleted; unknown shas are
/// ignored.
pub fn delete_commits(c: &mut Connection, shas: &[String]) -> Result<u64> {
    let tx = c.transaction()?;
    let mut deleted = 0u64;
    for sha in shas {
        for sql in DELETE_PER_COMMIT {
            tx.execute(sql, params![sha])?;
        }
        deleted += tx.execute("DELETE FROM commit_record WHERE sha = ?1", params![sha])? as u64;
    }
    tx.commit()?;
    Ok(deleted)
}
