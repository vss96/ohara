use anyhow::Result;
use ohara_core::storage::CommitRecord;
use ohara_core::types::CommitMeta;
use rusqlite::{params, Connection, OptionalExtension};

use crate::codec::vec_codec;

pub fn put(c: &mut Connection, record: &CommitRecord) -> Result<()> {
    let tx = c.transaction()?;
    // Resume safety: clear sqlite-vec + FTS5 mirrors first, then INSERT.
    // sqlite-vec's vec0 virtual tables do NOT honor `INSERT OR REPLACE`
    // — re-inserting the same `commit_sha` raises "UNIQUE constraint
    // failed on vec_commit primary key" even with OR REPLACE. So we
    // explicitly DELETE-then-INSERT, matching the pattern hunk::put_many
    // already uses for vec_hunk + fts_hunk_text. commit_record is a
    // regular table so OR REPLACE there is fine.
    tx.execute(
        "DELETE FROM vec_commit WHERE commit_sha = ?1",
        params![&record.meta.commit_sha],
    )?;
    tx.execute(
        "DELETE FROM fts_commit WHERE sha = ?1",
        params![&record.meta.commit_sha],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO commit_record (sha, parent_sha, is_merge, ts, author, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            &record.meta.commit_sha,
            &record.meta.parent_sha,
            record.meta.is_merge as i64,
            record.meta.ts,
            &record.meta.author,
            &record.meta.message,
        ],
    )?;
    let bytes = vec_codec::vec_to_bytes(&record.message_emb);
    tx.execute(
        "INSERT INTO vec_commit (commit_sha, message_emb) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, bytes],
    )?;
    tx.execute(
        "INSERT INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, &record.meta.message],
    )?;
    tx.commit()?;
    Ok(())
}

/// Cheap "is this commit already indexed?" PK lookup used by the
/// indexer's resume short-circuit (plan-9 / RFC v0.6.3). Returns true
/// when `commit_record.sha = ?1` exists. The lookup hits the primary-key
/// index, so cost is sub-millisecond per call — acceptable on cold
/// indexes too (a 5,800-commit walk pays ~0.1 s in PK lookups).
pub fn commit_exists(c: &Connection, sha: &str) -> Result<bool> {
    let found: Option<i64> = c
        .query_row(
            "SELECT 1 FROM commit_record WHERE sha = ?1 LIMIT 1",
            params![sha],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Fetch a single commit's metadata by SHA. Returns `Ok(None)` if the
/// SHA isn't present in `commit_record` (e.g., the commit is older than
/// the local index watermark). Used by the `explain_change` orchestrator
/// to enrich `git blame` results with author / message / timestamp for
/// display.
pub fn get(c: &Connection, sha: &str) -> Result<Option<CommitMeta>> {
    let row = c
        .query_row(
            "SELECT sha, parent_sha, is_merge, ts, author, message
             FROM commit_record
             WHERE sha = ?1",
            params![sha],
            |r| {
                let commit_sha: String = r.get(0)?;
                let parent_sha: Option<String> = r.get(1)?;
                let is_merge: i64 = r.get(2)?;
                let ts: i64 = r.get(3)?;
                let author: Option<String> = r.get(4)?;
                let message: String = r.get(5)?;
                Ok(CommitMeta {
                    commit_sha,
                    parent_sha,
                    is_merge: is_merge != 0,
                    author,
                    ts,
                    message,
                })
            },
        )
        .optional()?;
    Ok(row)
}
