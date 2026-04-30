use anyhow::Result;
use ohara_core::storage::CommitRecord;
use rusqlite::{params, Connection};

pub fn put(c: &Connection, record: &CommitRecord) -> Result<()> {
    c.execute(
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
    let bytes = vec_to_bytes(&record.message_emb);
    c.execute(
        "INSERT OR REPLACE INTO vec_commit (commit_sha, message_emb) VALUES (?1, ?2)",
        params![&record.meta.sha, bytes],
    )?;
    c.execute(
        "INSERT OR REPLACE INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.sha, &record.meta.message],
    )?;
    Ok(())
}

pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v { out.extend_from_slice(&f.to_le_bytes()); }
    out
}

pub fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}
