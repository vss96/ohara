//! Storage queries that back the `explain_change` orchestrator.
//!
//! The orchestrator (in `ohara-core::explain`) walks `git blame` lines
//! → unique commit SHAs, then asks storage for the per-commit hunks
//! that touched the queried file path. This module is the
//! file-scoped-per-commit hunk lookup; commit metadata comes from
//! `crate::tables::commit::get`.

use anyhow::Result;
use ohara_core::types::{CommitMeta, Hunk};
use rusqlite::{params, Connection};

use crate::codec::row_codec::str_to_change_kind;

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

/// Plan 12 Task 3.1: return commits that touched `file_path` near
/// `anchor_sha`, ordered by commit timestamp. Returns `(touched_hunk_count, CommitMeta)`
/// pairs so callers can show "this commit touched N hunks of the file".
///
/// `limit_before` and `limit_after` cap the per-side count
/// (asymmetric on purpose — `explain_change` callers usually want
/// more "what came after this code" context than "what came before").
/// File-scoped only — no semantic relatedness, just a cheap indexed
/// lookup against the existing `hunk` / `file_path` / `commit_record`
/// tables.
pub fn get_neighboring_file_commits(
    c: &Connection,
    file_path: &str,
    anchor_sha: &str,
    limit_before: u8,
    limit_after: u8,
) -> Result<Vec<(u32, CommitMeta)>> {
    // Resolve the anchor's timestamp first so we can split "before"
    // and "after" into two windows. If the anchor isn't indexed we
    // can't position it on the timeline; return an empty Vec.
    let anchor_ts: Option<i64> = c
        .query_row(
            "SELECT ts FROM commit_record WHERE sha = ?1",
            params![anchor_sha],
            |row| row.get(0),
        )
        .ok();
    let Some(anchor_ts) = anchor_ts else {
        return Ok(Vec::new());
    };

    let collect = |sql: &str, limit: u8| -> Result<Vec<(u32, CommitMeta)>> {
        let mut stmt = c.prepare(sql)?;
        let rows = stmt.query_map(
            params![file_path, anchor_sha, anchor_ts, limit as i64],
            |row| {
                let touched: i64 = row.get(0)?;
                let commit_sha: String = row.get(1)?;
                let parent_sha: Option<String> = row.get(2)?;
                let is_merge: i64 = row.get(3)?;
                let author: Option<String> = row.get(4)?;
                let ts: i64 = row.get(5)?;
                let message: String = row.get(6)?;
                Ok((
                    u32::try_from(touched).unwrap_or(0),
                    CommitMeta {
                        commit_sha,
                        parent_sha,
                        is_merge: is_merge != 0,
                        author,
                        ts,
                        message,
                    },
                ))
            },
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    };

    // Older commits (ts <= anchor_ts, sha != anchor), newest first.
    let older_sql = "
        SELECT COUNT(h.id), cr.sha, cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message
        FROM commit_record cr
        JOIN hunk h ON h.commit_sha = cr.sha
        JOIN file_path fp ON fp.id = h.file_path_id
        WHERE fp.path = ?1 AND cr.sha != ?2 AND cr.ts <= ?3
        GROUP BY cr.sha
        ORDER BY cr.ts DESC, cr.sha ASC
        LIMIT ?4";
    // Newer commits (ts > anchor_ts), oldest first (chronological after).
    let newer_sql = "
        SELECT COUNT(h.id), cr.sha, cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message
        FROM commit_record cr
        JOIN hunk h ON h.commit_sha = cr.sha
        JOIN file_path fp ON fp.id = h.file_path_id
        WHERE fp.path = ?1 AND cr.sha != ?2 AND cr.ts > ?3
        GROUP BY cr.sha
        ORDER BY cr.ts ASC, cr.sha ASC
        LIMIT ?4";

    let mut out = Vec::new();
    out.extend(collect(older_sql, limit_before)?);
    out.extend(collect(newer_sql, limit_after)?);
    Ok(out)
}
