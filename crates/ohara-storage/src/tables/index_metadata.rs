//! Plan 13 — `index_metadata` table helpers.
//!
//! `get` reads every component row for a repo; `put_many` upserts the
//! caller-supplied `(component, version)` pairs without touching
//! components the caller didn't pass. Both helpers run inside the
//! `with_conn` closure pattern shared with the rest of the storage
//! backend.

use anyhow::Result;
use ohara_core::index_metadata::StoredIndexMetadata;
use rusqlite::{params, Connection};

/// Read all stored components for a repo as a typed
/// `StoredIndexMetadata`. Returns an empty map for repos that have no
/// rows yet (e.g. an in-memory test DB before any indexing pass).
pub fn get(c: &Connection, repo_id: &str) -> Result<StoredIndexMetadata> {
    let mut stmt = c.prepare(
        "SELECT component, version FROM index_metadata WHERE repo_id = ?1 ORDER BY component",
    )?;
    let rows = stmt.query_map(params![repo_id], |r| {
        let component: String = r.get(0)?;
        let version: String = r.get(1)?;
        Ok((component, version))
    })?;
    let mut stored = StoredIndexMetadata::default();
    for row in rows {
        let (component, version) = row?;
        stored.components.insert(component, version);
    }
    Ok(stored)
}

/// Upsert every `(component, version)` pair for `repo_id`. Each row's
/// `recorded_at` is set to the current unix time; `value_json` is left
/// at the schema's `'{}'` default until a future plan needs it.
///
/// Scoped replacement: rows for components NOT in `components` are
/// left untouched. The caller is responsible for passing the full set
/// of components they want to refresh.
pub fn put_many(c: &mut Connection, repo_id: &str, components: &[(String, String)]) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let tx = c.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO index_metadata (repo_id, component, version, value_json, recorded_at)
             VALUES (?1, ?2, ?3, '{}', ?4)
             ON CONFLICT(repo_id, component) DO UPDATE SET
               version = excluded.version,
               value_json = excluded.value_json,
               recorded_at = excluded.recorded_at",
        )?;
        for (component, version) in components {
            stmt.execute(params![repo_id, component, version, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}
