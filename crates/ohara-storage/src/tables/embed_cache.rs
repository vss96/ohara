//! Plan-27 chunk-level embed cache. Maps
//! `(content_hash, embed_model)` → 384-float vector. Used by
//! `EmbedStage` to skip re-embedding identical chunk content.

use anyhow::Result;
use ohara_core::types::ContentHash;
use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::codec::vec_codec::{bytes_to_vec, vec_to_bytes};

/// Look up cached embeddings for a batch of `(content_hash, embed_model)`
/// keys. Returns one map entry per hit; misses are absent.
pub fn get_many(
    c: &Connection,
    hashes: &[ContentHash],
    embed_model: &str,
) -> Result<HashMap<ContentHash, Vec<f32>>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; hashes.len()].join(",");
    let sql = format!(
        "SELECT content_hash, diff_emb FROM chunk_embed_cache \
         WHERE embed_model = ? AND content_hash IN ({placeholders})"
    );
    let mut stmt = c.prepare(&sql)?;
    let mut bindings: Vec<rusqlite::types::Value> = Vec::with_capacity(hashes.len() + 1);
    bindings.push(rusqlite::types::Value::Text(embed_model.to_owned()));
    for h in hashes {
        bindings.push(rusqlite::types::Value::Text(h.as_str().to_owned()));
    }
    let rows = stmt.query_map(rusqlite::params_from_iter(&bindings), |row| {
        let key: String = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        Ok((key, blob))
    })?;
    let mut out = HashMap::with_capacity(hashes.len());
    for r in rows {
        let (key, blob) = r?;
        let v = bytes_to_vec(&blob);
        out.insert(ContentHash::from_hex(&key), v);
    }
    Ok(out)
}

/// Insert one row per `(hash, embed_model)` entry. Existing rows are
/// preserved via `INSERT OR IGNORE` — the cache is content-addressed
/// so a re-insert of the same key + model would be a no-op anyway.
pub fn put_many(
    c: &mut Connection,
    entries: &[(ContentHash, Vec<f32>)],
    embed_model: &str,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let tx = c.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO chunk_embed_cache \
             (content_hash, embed_model, diff_emb) VALUES (?1, ?2, ?3)",
        )?;
        for (hash, vec) in entries {
            let bytes = vec_to_bytes(vec);
            stmt.execute(params![hash.as_str(), embed_model, bytes])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStorage;
    use ohara_core::types::ContentHash;

    async fn temp_storage() -> SqliteStorage {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embed_cache_test.db");
        // Leak the tempdir handle for the duration of the test; tests
        // are short-lived and the OS reclaims on process exit.
        Box::leak(Box::new(dir));
        SqliteStorage::open(&path).await.unwrap()
    }

    #[tokio::test]
    async fn get_returns_empty_map_when_cache_is_empty() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hashes = vec![ContentHash::from_hex("a"), ContentHash::from_hex("b")];
        let out = conn
            .interact(move |c| get_many(c, &hashes, "model-x"))
            .await
            .unwrap()
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn put_then_get_round_trips_a_single_entry() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hash = ContentHash::from_hex("deadbeef");
        let vec = vec![0.1_f32, 0.2, 0.3, 0.4];
        let entries = vec![(hash.clone(), vec.clone())];
        let model = "model-x".to_string();
        let model_for_get = model.clone();
        let hashes_for_get = vec![hash.clone()];
        conn.interact(move |c| put_many(c, &entries, &model))
            .await
            .unwrap()
            .unwrap();
        let out = conn
            .interact(move |c| get_many(c, &hashes_for_get, &model_for_get))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.len(), 1);
        let got = out.get(&hash).unwrap();
        assert_eq!(got, &vec);
    }

    #[tokio::test]
    async fn same_hash_different_model_are_distinct_rows() {
        let storage = temp_storage().await;
        let conn = storage.pool().get().await.unwrap();
        let hash = ContentHash::from_hex("aa");
        let v1 = vec![1.0_f32];
        let v2 = vec![2.0_f32];
        let entries_a = vec![(hash.clone(), v1.clone())];
        let entries_b = vec![(hash.clone(), v2.clone())];
        conn.interact(move |c| put_many(c, &entries_a, "model-a"))
            .await
            .unwrap()
            .unwrap();
        conn.interact(move |c| put_many(c, &entries_b, "model-b"))
            .await
            .unwrap()
            .unwrap();
        let h1 = vec![hash.clone()];
        let h2 = vec![hash.clone()];
        let from_a = conn
            .interact(move |c| get_many(c, &h1, "model-a"))
            .await
            .unwrap()
            .unwrap();
        let from_b = conn
            .interact(move |c| get_many(c, &h2, "model-b"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from_a.get(&hash), Some(&v1));
        assert_eq!(from_b.get(&hash), Some(&v2));
    }
}
