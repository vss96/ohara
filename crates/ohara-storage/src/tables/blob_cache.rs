use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

pub fn was_seen(c: &Connection, blob_sha: &str, model: &str) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM blob_cache WHERE blob_sha = ?1 AND embedding_model = ?2",
        params![blob_sha, model],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn record(c: &Connection, blob_sha: &str, model: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    c.execute(
        "INSERT OR REPLACE INTO blob_cache (blob_sha, embedding_model, embedded_at) VALUES (?1, ?2, ?3)",
        params![blob_sha, model, now],
    )?;
    Ok(())
}
