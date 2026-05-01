use anyhow::Result;
use ohara_core::storage::{HunkHit, HunkRecord};
use ohara_core::types::{CommitMeta, Hunk};
use rusqlite::{params, Connection};

use crate::row_codec::{change_kind_to_str, str_to_change_kind, upsert_file_path};
use crate::vec_codec::vec_to_bytes;

pub fn put_many(c: &mut Connection, records: &[HunkRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let tx = c.transaction()?;
    // Resume safety: drop any hunks (and their FTS5 + vec mirrors) that
    // already exist for the commits we're about to write. The indexer
    // calls put_many once per commit, but this also handles batched
    // callers. Without this, an interrupted prior run that completed
    // put_commit + put_hunks for some commits would, on resume, see
    // those commits' hunks doubled (no UNIQUE constraint on hunk).
    let mut shas: Vec<String> = records.iter().map(|r| r.hunk.commit_sha.clone()).collect();
    shas.sort();
    shas.dedup();
    for sha in &shas {
        tx.execute(
            "DELETE FROM fts_hunk_text WHERE hunk_id IN \
             (SELECT id FROM hunk WHERE commit_sha = ?1)",
            params![sha],
        )?;
        tx.execute(
            "DELETE FROM vec_hunk WHERE hunk_id IN \
             (SELECT id FROM hunk WHERE commit_sha = ?1)",
            params![sha],
        )?;
        tx.execute("DELETE FROM hunk WHERE commit_sha = ?1", params![sha])?;
    }
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
        // Keep the FTS5 hunk-text index in lockstep with the hunk table so
        // BM25 lane queries see new rows immediately.
        tx.execute(
            "INSERT INTO fts_hunk_text (hunk_id, content) VALUES (?1, ?2)",
            params![hunk_id, &r.hunk.diff_text],
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
        "SELECT h.id, h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
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
    let bind_refs: Vec<(&str, &dyn rusqlite::ToSql)> = binds
        .iter()
        .map(|(k, v)| (*k, v.as_ref() as &dyn rusqlite::ToSql))
        .collect();

    let rows = stmt.query_map(bind_refs.as_slice(), |row| {
        let hunk_id: i64 = row.get(0)?;
        let commit_sha: String = row.get(1)?;
        let file_path: String = row.get(2)?;
        let language: Option<String> = row.get(3)?;
        let change_kind_s: String = row.get(4)?;
        let diff_text: String = row.get(5)?;
        let parent_sha: Option<String> = row.get(6)?;
        let is_merge: i64 = row.get(7)?;
        let author: Option<String> = row.get(8)?;
        let ts: i64 = row.get(9)?;
        let message: String = row.get(10)?;
        let distance: f32 = row.get(11)?;

        let change_kind = str_to_change_kind(&change_kind_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?;
        let hunk = Hunk {
            commit_sha: commit_sha.clone(),
            file_path,
            language,
            change_kind,
            diff_text,
        };
        let commit = CommitMeta {
            commit_sha,
            parent_sha,
            is_merge: is_merge != 0,
            author,
            ts,
            message,
        };
        // Distance from sqlite-vec is L2; smaller is closer. Convert to a
        // similarity-like score where larger is better.
        let similarity = 1.0 / (1.0 + distance);
        Ok(HunkHit {
            hunk_id,
            hunk,
            commit,
            similarity,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// BM25 over `fts_hunk_text`, joined to the hunk's file + commit so we
/// return the same `HunkHit` shape as `knn`. Ordered best-first.
pub fn bm25_by_text(
    c: &Connection,
    query: &str,
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");

    // SQLite's bm25() returns a negative number where most-negative is best.
    // ORDER BY bm25(fts_hunk_text) ASC puts the strongest match first.
    let sql = format!(
        "SELECT h.id, h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message,
                bm25(fts_hunk_text) AS rank_score
         FROM fts_hunk_text f
         JOIN hunk h ON h.id = f.hunk_id
         JOIN file_path fp ON fp.id = h.file_path_id
         JOIN commit_record cr ON cr.sha = h.commit_sha
         WHERE fts_hunk_text MATCH :query
           {ts_filter} {lang_filter}
         ORDER BY rank_score ASC
         LIMIT :k"
    );

    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = Vec::new();
    binds.push((":query", Box::new(query.to_string())));
    binds.push((":k", Box::new(k as i64)));
    if let Some(lang) = language {
        binds.push((":lang", Box::new(lang.to_string())));
    }
    if let Some(ts) = since_unix {
        binds.push((":ts", Box::new(ts)));
    }

    let mut stmt = c.prepare(&sql)?;
    let bind_refs: Vec<(&str, &dyn rusqlite::ToSql)> = binds
        .iter()
        .map(|(k, v)| (*k, v.as_ref() as &dyn rusqlite::ToSql))
        .collect();

    let rows = stmt.query_map(bind_refs.as_slice(), |row| {
        let hunk_id: i64 = row.get(0)?;
        let commit_sha: String = row.get(1)?;
        let file_path: String = row.get(2)?;
        let language: Option<String> = row.get(3)?;
        let change_kind_s: String = row.get(4)?;
        let diff_text: String = row.get(5)?;
        let parent_sha: Option<String> = row.get(6)?;
        let is_merge: i64 = row.get(7)?;
        let author: Option<String> = row.get(8)?;
        let ts: i64 = row.get(9)?;
        let message: String = row.get(10)?;
        let rank_score: f64 = row.get(11)?;

        let change_kind = str_to_change_kind(&change_kind_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?;
        let hunk = Hunk {
            commit_sha: commit_sha.clone(),
            file_path,
            language,
            change_kind,
            diff_text,
        };
        let commit = CommitMeta {
            commit_sha,
            parent_sha,
            is_merge: is_merge != 0,
            author,
            ts,
            message,
        };
        // Negate -> positive normalized "higher is better" score, same shape
        // as `knn`'s similarity. Callers should treat this as informational;
        // ranking is done by RRF on the row order, not on this number.
        let similarity = 1.0 / (1.0 + (-rank_score) as f32);
        Ok(HunkHit {
            hunk_id,
            hunk,
            commit,
            similarity,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
