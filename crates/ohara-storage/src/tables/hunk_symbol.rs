//! Plan 11 — `hunk_symbol` table helpers.
//!
//! Stores per-hunk symbol attribution (which symbol(s) inside the
//! file did this hunk actually touch) keyed by `(hunk_id, symbol_kind,
//! symbol_name)`. Used by the new `bm25_hunks_by_historical_symbol`
//! retrieval lane (plan 11 Task 4.1) to point symbol-name queries at
//! hunks that genuinely touched the symbol, not just at every hunk in
//! a file that happens to contain it.

use anyhow::Result;
use ohara_core::types::{AttributionKind, CommitMeta, Hunk, HunkSymbol, SymbolKind};
use rusqlite::{params, Connection, Transaction};
use std::str::FromStr;

use crate::codec::row_codec::str_to_change_kind;

/// Insert one row per `HunkSymbol` for `hunk_id`. The caller is
/// responsible for clearing prior rows when re-indexing the same
/// hunk — `put_many` in `tables/hunk` does this via the
/// ON DELETE CASCADE on the foreign key.
pub fn put_for_hunk(tx: &Transaction, hunk_id: i64, symbols: &[HunkSymbol]) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "INSERT OR REPLACE INTO hunk_symbol \
         (hunk_id, symbol_kind, symbol_name, qualified_name, attribution_kind) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for sym in symbols {
        stmt.execute(params![
            hunk_id,
            symbol_kind_to_str(sym.kind),
            &sym.name,
            &sym.qualified_name,
            sym.attribution.as_str(),
        ])?;
    }
    Ok(())
}

/// Plan 11 Task 4.1: return hunks attributed to a symbol whose name
/// matches `query` (LIKE-style substring match, case-insensitive).
/// Joined to commit + file rows to return the same `HunkHit` shape as
/// the FTS lanes, ordered by attribution-confidence then commit
/// recency (newest first).
pub fn bm25_by_historical_symbol(
    c: &Connection,
    query: &str,
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<ohara_core::storage::HunkHit>> {
    // Substring match keeps the lane forgiving — a query for
    // "retry_with_backoff" will hit `retry_with_backoff_v2` too.
    // The ORDER BY favors ExactSpan over HunkHeader so the highest-
    // confidence rows surface first; ts DESC breaks the tie.
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");
    let sql = format!(
        "SELECT h.id, h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text, \
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message, hs.attribution_kind \
         FROM hunk_symbol hs \
         JOIN hunk h ON h.id = hs.hunk_id \
         JOIN file_path fp ON fp.id = h.file_path_id \
         JOIN commit_record cr ON cr.sha = h.commit_sha \
         WHERE LOWER(hs.symbol_name) LIKE :q \
           {ts_filter} {lang_filter} \
         ORDER BY \
           CASE hs.attribution_kind \
             WHEN 'exact_span' THEN 0 \
             WHEN 'hunk_header' THEN 1 \
             ELSE 2 \
           END ASC, \
           cr.ts DESC \
         LIMIT :k"
    );

    let needle = format!("%{}%", query.to_lowercase());
    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = Vec::new();
    binds.push((":q", Box::new(needle)));
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

    let mut out: Vec<ohara_core::storage::HunkHit> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
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
        let attribution_s: String = row.get(11)?;
        let change_kind = str_to_change_kind(&change_kind_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?;
        // attribution_kind decides the similarity score: exact spans
        // are most trustworthy, header-only is somewhat trustworthy.
        let similarity = match attribution_s.as_str() {
            "exact_span" => 1.0_f32,
            "hunk_header" => 0.6,
            _ => 0.3,
        };
        Ok(ohara_core::storage::HunkHit {
            hunk_id,
            hunk: Hunk {
                commit_sha: commit_sha.clone(),
                file_path,
                language,
                change_kind,
                diff_text,
            },
            commit: CommitMeta {
                commit_sha,
                parent_sha,
                is_merge: is_merge != 0,
                author,
                ts,
                message,
            },
            similarity,
        })
    })?;

    for r in rows {
        let hit = r?;
        // A single hunk can attribute to multiple symbols, all of
        // which match the query — dedupe by hunk_id so the returned
        // ranking has one row per hunk.
        if seen.insert(hit.hunk_id) {
            out.push(hit);
        }
    }
    Ok(out)
}

/// Plan 11: fetch all `HunkSymbol` rows for a given hunk id. Used by
/// the retriever (Task 4.2) to populate `PatternHit.related_head_symbols`.
/// Returns an empty Vec for hunks with no attribution rows; that's a
/// legitimate state for hunks indexed before V4.
pub fn get_for_hunk(c: &Connection, hunk_id: i64) -> Result<Vec<HunkSymbol>> {
    let mut stmt = c.prepare(
        "SELECT symbol_kind, symbol_name, qualified_name, attribution_kind \
         FROM hunk_symbol WHERE hunk_id = ?1 \
         ORDER BY \
           CASE attribution_kind \
             WHEN 'exact_span' THEN 0 \
             WHEN 'hunk_header' THEN 1 \
             ELSE 2 \
           END ASC, symbol_name ASC",
    )?;
    let rows = stmt.query_map(params![hunk_id], |row| {
        let kind_s: String = row.get(0)?;
        let name: String = row.get(1)?;
        let qualified_name: Option<String> = row.get(2)?;
        let attribution_s: String = row.get(3)?;
        let kind = str_to_symbol_kind(&kind_s).unwrap_or(SymbolKind::Function);
        let attribution =
            AttributionKind::from_str(&attribution_s).unwrap_or(AttributionKind::HunkHeader);
        Ok(HunkSymbol {
            kind,
            name,
            qualified_name,
            attribution,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn symbol_kind_to_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Const => "const",
    }
}

fn str_to_symbol_kind(s: &str) -> Option<SymbolKind> {
    match s {
        "function" => Some(SymbolKind::Function),
        "method" => Some(SymbolKind::Method),
        "class" => Some(SymbolKind::Class),
        "const" => Some(SymbolKind::Const),
        _ => None,
    }
}
