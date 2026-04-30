use anyhow::Result;
use ohara_core::storage::{HunkHit, HunkRecord};
use ohara_core::types::{ChangeKind, CommitMeta, Hunk};
use rusqlite::{params, Connection};

use crate::vec_codec::vec_to_bytes;

pub fn put_many(c: &mut Connection, records: &[HunkRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let tx = c.transaction()?;
    for r in records {
        let fp_id = upsert_file_path(&tx, &r.hunk.file_path, r.hunk.language.as_deref())?;
        tx.execute(
            "INSERT INTO hunk (commit_sha, file_path_id, change_kind, diff_text)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &r.hunk.commit_sha,
                fp_id,
                change_kind_to_str(r.hunk.change_kind),
                &r.hunk.diff_text,
            ],
        )?;
        let hunk_id: i64 = tx.last_insert_rowid();
        let bytes = vec_to_bytes(&r.diff_emb);
        tx.execute(
            "INSERT INTO vec_hunk (hunk_id, diff_emb) VALUES (?1, ?2)",
            params![hunk_id, bytes],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn knn(
    c: &Connection,
    query_emb: &[f32],
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let qbytes = vec_to_bytes(query_emb);
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");
    let sql = format!(
        "SELECT h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message,
                v.distance
         FROM vec_hunk v
         JOIN hunk h ON h.id = v.hunk_id
         JOIN file_path fp ON fp.id = h.file_path_id
         JOIN commit_record cr ON cr.sha = h.commit_sha
         WHERE v.diff_emb MATCH :emb AND k = :k
         {ts_filter} {lang_filter}
         ORDER BY v.distance ASC"
    );

    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = Vec::new();
    binds.push((":emb", Box::new(qbytes)));
    binds.push((":k", Box::new(k as i64)));
    if let Some(lang) = language {
        binds.push((":lang", Box::new(lang.to_string())));
    }
    if let Some(ts) = since_unix {
        binds.push((":ts", Box::new(ts)));
    }

    let mut stmt = c.prepare(&sql)?;
    let bind_refs: Vec<(&str, &dyn rusqlite::ToSql)> =
        binds.iter().map(|(k, v)| (*k, v.as_ref() as &dyn rusqlite::ToSql)).collect();

    let rows = stmt.query_map(bind_refs.as_slice(), |row| {
        let commit_sha: String = row.get(0)?;
        let file_path: String = row.get(1)?;
        let language: Option<String> = row.get(2)?;
        let change_kind_s: String = row.get(3)?;
        let diff_text: String = row.get(4)?;
        let parent_sha: Option<String> = row.get(5)?;
        let is_merge: i64 = row.get(6)?;
        let author: Option<String> = row.get(7)?;
        let ts: i64 = row.get(8)?;
        let message: String = row.get(9)?;
        let distance: f32 = row.get(10)?;

        let hunk = Hunk {
            commit_sha: commit_sha.clone(),
            file_path,
            language,
            change_kind: str_to_change_kind(&change_kind_s),
            diff_text,
        };
        let commit = CommitMeta {
            sha: commit_sha,
            parent_sha,
            is_merge: is_merge != 0,
            author,
            ts,
            message,
        };
        // Distance from sqlite-vec is L2; smaller is closer. Convert to a
        // similarity-like score where larger is better.
        let similarity = 1.0 / (1.0 + distance);
        Ok(HunkHit { hunk, commit, similarity })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn upsert_file_path(c: &Connection, path: &str, language: Option<&str>) -> Result<i64> {
    c.execute(
        "INSERT INTO file_path (path, language, active) VALUES (?1, ?2, 1)
         ON CONFLICT(path) DO UPDATE SET language = COALESCE(excluded.language, file_path.language)",
        params![path, language],
    )?;
    let id: i64 = c.query_row(
        "SELECT id FROM file_path WHERE path = ?1",
        params![path],
        |r| r.get(0),
    )?;
    Ok(id)
}

fn change_kind_to_str(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

fn str_to_change_kind(s: &str) -> ChangeKind {
    match s {
        "added" => ChangeKind::Added,
        "modified" => ChangeKind::Modified,
        "deleted" => ChangeKind::Deleted,
        "renamed" => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}
