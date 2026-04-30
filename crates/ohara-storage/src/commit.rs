use anyhow::Result;
use ohara_core::storage::CommitRecord;
use rusqlite::{params, Connection};

use crate::vec_codec;

pub fn put(c: &mut Connection, record: &CommitRecord) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO commit_record (sha, parent_sha, is_merge, ts, author, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            &record.meta.sha,
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
        params![&record.meta.sha, bytes],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.sha, &record.meta.message],
    )?;
    tx.commit()?;
    Ok(())
}
