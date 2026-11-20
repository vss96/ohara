//! Deleting commits that are no longer reachable from any ref or worktree
//! HEAD (plan 33).
//!
//! The content-addressed `chunk_embed_cache`, the shared
//! `file_path` dictionary and the HEAD-symbol snapshot (`symbol`,
//! `vec_symbol`, `fts_symbol*`) are not keyed by commit and are kept.

use anyhow::Result;
use rusqlite::{params_from_iter, Connection};

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

/// Statements run once per batch, in order; `{in}` is the batch's
/// `?, ?, …` placeholder list. Hunk mirrors go before `hunk` (their
/// sub-select needs the hunk rows); `hunk` goes before `commit_record`
/// (foreign key); `hunk_symbol` rows go with `hunk` via `ON DELETE CASCADE`.
/// The FTS5 `hunk_id` / `sha` columns are `UNINDEXED`, so each FTS
/// statement scans its table: one scan per batch, not per sha.
const DELETE_PER_BATCH: [&str; 6] = [
    "DELETE FROM fts_hunk_text WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha IN ({in}))",
    "DELETE FROM fts_hunk_semantic WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha IN ({in}))",
    "DELETE FROM vec_hunk WHERE hunk_id IN (SELECT id FROM hunk WHERE commit_sha IN ({in}))",
    "DELETE FROM hunk WHERE commit_sha IN ({in})",
    "DELETE FROM vec_commit WHERE commit_sha IN ({in})",
    "DELETE FROM fts_commit WHERE sha IN ({in})",
];

/// One transaction per call: vec_commit, fts_commit, the hunks' fts_hunk_text /
/// fts_hunk_semantic / vec_hunk rows, hunk (hunk_symbol cascades), commit_record.
///
/// Returns the number of `commit_record` rows deleted; unknown shas are
/// ignored. Callers pass bounded batches (prune uses 500).
pub fn delete_commits(c: &mut Connection, shas: &[String]) -> Result<u64> {
    if shas.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; shas.len()].join(", ");
    let tx = c.transaction()?;
    for sql in DELETE_PER_BATCH {
        tx.execute(&sql.replace("{in}", &placeholders), params_from_iter(shas))?;
    }
    let deleted = tx.execute(
        &format!("DELETE FROM commit_record WHERE sha IN ({placeholders})"),
        params_from_iter(shas),
    )? as u64;
    tx.commit()?;
    Ok(deleted)
}
