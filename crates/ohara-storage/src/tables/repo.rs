use crate::tables::indexed_tip;
use anyhow::Result;
use chrono::Utc;
use ohara_core::query::IndexStatus;
use rusqlite::{params, Connection};

pub fn upsert(c: &Connection, id: &str, path: &str, first_commit_sha: &str) -> Result<()> {
    c.execute(
        "INSERT INTO repo (id, path, first_commit_sha, last_indexed_commit, indexed_at, schema_version)
         VALUES (?1, ?2, ?3, NULL, NULL, 1)
         ON CONFLICT(id) DO UPDATE SET path = excluded.path",
        params![id, path, first_commit_sha],
    )?;
    Ok(())
}

/// Status of the repo, read from the main (`''`) tip plus `repo.indexed_at`.
pub fn get_status(c: &Connection, id: &str) -> Result<IndexStatus> {
    let row: Option<(Option<String>, Option<String>)> = c
        .query_row(
            "SELECT t.sha, r.indexed_at FROM repo r
             LEFT JOIN indexed_tip t ON t.repo_id = r.id AND t.worktree_key = ''
             WHERE r.id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (last_indexed_commit, indexed_at) = row.unwrap_or((None, None));
    Ok(IndexStatus {
        last_indexed_commit,
        commits_behind_head: 0, // computed by the caller from git rev-list
        indexed_at,
    })
}

/// Records `sha` as the main tip and bumps `repo.indexed_at`, atomically.
pub fn set_watermark(c: &mut Connection, id: &str, sha: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let tx = c.transaction()?;
    indexed_tip::upsert(&tx, id, "", sha)?;
    tx.execute(
        "UPDATE repo SET indexed_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    tx.commit()?;
    Ok(())
}
