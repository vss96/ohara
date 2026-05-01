use anyhow::Result;
use ohara_core::storage::CommitRecord;
use ohara_core::types::CommitMeta;
use rusqlite::{params, Connection, OptionalExtension};

use crate::codec::vec_codec;

pub fn put(c: &mut Connection, record: &CommitRecord) -> Result<()> {
    let tx = c.transaction()?;
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
        "INSERT OR REPLACE INTO vec_commit (commit_sha, message_emb) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, bytes],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, &record.meta.message],
    )?;
    tx.commit()?;
    Ok(())
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
