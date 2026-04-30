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

pub fn get_status(c: &Connection, id: &str) -> Result<IndexStatus> {
    let row: Option<(Option<String>, Option<String>)> = c
        .query_row(
            "SELECT last_indexed_commit, indexed_at FROM repo WHERE id = ?1",
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

pub fn set_watermark(c: &Connection, id: &str, sha: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    c.execute(
        "UPDATE repo SET last_indexed_commit = ?2, indexed_at = ?3 WHERE id = ?1",
        params![id, sha, now],
    )?;
    Ok(())
}
