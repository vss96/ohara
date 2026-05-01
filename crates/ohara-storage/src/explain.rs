//! Storage queries that back the `explain_change` orchestrator.
//!
//! The orchestrator (in `ohara-core::explain`) walks `git blame` lines
//! → unique commit SHAs, then asks storage for the per-commit hunks
//! that touched the queried file path. This module is the
//! file-scoped-per-commit hunk lookup; commit metadata comes from
//! `crate::commit::get`.

use anyhow::Result;
use ohara_core::types::Hunk;
use rusqlite::{params, Connection};

use crate::row_codec::str_to_change_kind;

/// Return the hunks of `sha` whose `file_path` equals `file_path`.
/// Returns an empty Vec if the commit isn't indexed or didn't touch
/// the requested path. Mirrors the `Storage::get_hunks_for_file_in_commit`
/// trait contract.
pub fn get_hunks_for_file_in_commit(
    c: &Connection,
    sha: &str,
    file_path: &str,
) -> Result<Vec<Hunk>> {
    let mut stmt = c.prepare(
        "SELECT h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text
         FROM hunk h
         JOIN file_path fp ON fp.id = h.file_path_id
         WHERE h.commit_sha = ?1 AND fp.path = ?2
         ORDER BY h.id ASC",
    )?;
    let rows = stmt.query_map(params![sha, file_path], |row| {
        let commit_sha: String = row.get(0)?;
        let file_path: String = row.get(1)?;
        let language: Option<String> = row.get(2)?;
        let change_kind_s: String = row.get(3)?;
        let diff_text: String = row.get(4)?;
        let change_kind = str_to_change_kind(&change_kind_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
        })?;
        Ok(Hunk {
            commit_sha,
            file_path,
            language,
            change_kind,
            diff_text,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

