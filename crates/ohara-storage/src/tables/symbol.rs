//! Symbol persistence + BM25 lookup over `fts_symbol_name`.
//!
//! Persists each `Symbol` to the `symbol` table and mirrors it into the
//! `fts_symbol_name` virtual table so the BM25-by-symbol-name retrieval
//! lane has rows to match against. `Symbol::sibling_names` is serialized
//! as JSON so the FTS row indexes the merged-sibling names produced by
//! the AST sibling-merge chunker.

use anyhow::Result;
use ohara_core::storage::HunkHit;
use ohara_core::types::{CommitMeta, Hunk, Symbol, SymbolKind};
use rusqlite::{params, Connection};

use crate::codec::row_codec::{str_to_change_kind, upsert_file_path};

/// Oversample factor for the BM25-by-symbol-name lane's `LIMIT` (issue #57).
///
/// The lane joins `fts_symbol_name` to every hunk that ever touched the
/// matched symbol's file, so one matched symbol can fan out to hundreds
/// of rows on hot files. Without a `LIMIT`, SQLite's
/// `TEMP B-TREE FOR ORDER BY` materialises the full fan-out before Rust
/// sees a row; the dedup-by-first-seen loop in `bm25_by_name` then
/// throws most of them away. Bounding the temp B-tree fill at
/// `k * SYMBOL_LANE_OVERSAMPLE` rows keeps SQLite's work proportional
/// to the caller's `k` while still leaving Rust's dedup enough rows to
/// pick distinct hunks from. 10x is a deliberately generous slack:
/// catastrophic recall loss would only happen if a single hunk produced
/// more than 10 matched symbols ahead of the rank-`k+1` distinct hunk,
/// which the indexer's per-file dedup already rules out.
pub(crate) const SYMBOL_LANE_OVERSAMPLE: i64 = 10;

/// Persist a single `Symbol` to the `symbol` table and mirror it into
/// the `fts_symbol_name` virtual table. Caller owns the transaction.
fn put_one(tx: &rusqlite::Transaction<'_>, fp_id: i64, s: &Symbol) -> Result<()> {
    // Serialize the sibling names produced by the AST chunker (Track C).
    // For single-symbol chunks this is "[]", matching the SQL default.
    let sibling_json = serde_json::to_string(&s.sibling_names)?;
    tx.execute(
        "INSERT INTO symbol (file_path_id, kind, name, qualified_name,
                             span_start, span_end, blob_sha, source_text,
                             sibling_names)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            fp_id,
            symbol_kind_to_str(&s.kind),
            &s.name,
            &s.qualified_name,
            s.span_start as i64,
            s.span_end as i64,
            &s.blob_sha,
            &s.source_text,
            sibling_json,
        ],
    )?;
    let symbol_id: i64 = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO fts_symbol_name (symbol_id, kind, name, sibling_names)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            symbol_id,
            symbol_kind_to_str(&s.kind),
            &s.name,
            sibling_json,
        ],
    )?;
    Ok(())
}

/// Drop all symbol rows (and their FTS5 mirror) for the repo's HEAD
/// snapshot. `ohara index --force` calls this before re-running
/// `put_head_symbols` so repeated re-extracts don't accumulate
/// duplicate rows. The schema-level cascade also clears the related
/// `vec_symbol` rows.
pub fn clear_all(c: &mut Connection) -> Result<()> {
    let tx = c.transaction()?;
    // Drop FTS5 + vec mirrors first (they reference symbol.id), then symbol.
    // V1 has fts_symbol (qualified_name + source_text); V2 added
    // fts_symbol_name (kind + name + sibling_names); both must clear.
    tx.execute("DELETE FROM fts_symbol", [])?;
    tx.execute("DELETE FROM fts_symbol_name", [])?;
    tx.execute("DELETE FROM vec_symbol", [])?;
    tx.execute("DELETE FROM symbol", [])?;
    tx.commit()?;
    Ok(())
}

/// Replace HEAD-frame symbols for a repo. The `symbol` table holds only
/// the latest HEAD snapshot — historical symbols are never kept — so
/// every call atomically clears existing rows (and their FTS5 + vec
/// mirrors) before inserting the new set. Without this, every regular
/// `ohara index` (and every post-commit hook fire) would append a
/// fresh duplicate set, causing the table to grow linearly with index
/// runs.
pub fn put_many(c: &mut Connection, symbols: &[Symbol]) -> Result<()> {
    let tx = c.transaction()?;
    // Clear stale HEAD symbols + their mirrors first. Order matters:
    // FTS5 + vec virtual tables hold rowid references into `symbol`,
    // so they're cleared before the parent rows.
    tx.execute("DELETE FROM fts_symbol", [])?;
    tx.execute("DELETE FROM fts_symbol_name", [])?;
    tx.execute("DELETE FROM vec_symbol", [])?;
    tx.execute("DELETE FROM symbol", [])?;
    for s in symbols {
        let fp_id = upsert_file_path(&tx, &s.file_path, Some(&s.language))?;
        put_one(&tx, fp_id, s)?;
    }
    tx.commit()?;
    Ok(())
}

/// BM25 over `fts_symbol_name`, joined to the touched-file's hunk +
/// commit metadata so we return the same `HunkHit` shape as the dense
/// vector lane. SQLite's `bm25(<table>)` function only resolves inside
/// a query that *directly* targets the FTS5 virtual table (otherwise
/// SQLite errors with "unable to use function bm25 in the requested
/// context"), so we compute the per-symbol score in an inline subquery
/// and aggregate per hunk in the outer query.
pub fn bm25_by_name(
    c: &Connection,
    query: &str,
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let sql = build_bm25_sql(language, since_unix);
    // Issue #57: bound SQLite's TEMP B-TREE FOR ORDER BY at
    // k * SYMBOL_LANE_OVERSAMPLE rows so the dedup-in-Rust loop below
    // still gets enough candidates to pick from without materialising
    // the full hunk fan-out for hot-file queries. Cast through i64 so
    // u8::MAX * 10 (= 2550) cannot wrap.
    let k_oversample: i64 = i64::from(k) * SYMBOL_LANE_OVERSAMPLE;

    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = Vec::new();
    binds.push((
        ":query",
        Box::new(crate::tables::hunk::sanitize_fts5_query(query)),
    ));
    if let Some(lang) = language {
        binds.push((":lang", Box::new(lang.to_string())));
    }
    if let Some(ts) = since_unix {
        binds.push((":ts", Box::new(ts)));
    }
    binds.push((":k_oversample", Box::new(k_oversample)));

    let mut stmt = c.prepare(&sql)?;
    let bind_refs: Vec<(&str, &dyn rusqlite::ToSql)> = binds
        .iter()
        .map(|(k, v)| (*k, v.as_ref() as &dyn rusqlite::ToSql))
        .collect();

    let rows = stmt.query_map(bind_refs.as_slice(), row_to_hit)?;
    // De-duplicate by hunk id keeping the first (best-BM25) occurrence,
    // then truncate to k. Multiple symbols in the same hunk's file can
    // match a single query; we want one row per hunk.
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let hit = r?;
        if seen.insert(hit.hunk_id) {
            out.push(hit);
            if out.len() == k as usize {
                break;
            }
        }
    }
    Ok(out)
}

/// Build the BM25-by-symbol-name SQL string. Extracted so unit tests can
/// pin invariants of the query (LIMIT clause, clause ordering) without
/// having to construct an FTS5 fixture for every assertion.
///
/// SQLite's bm25() must be used in a SELECT that directly references the
/// FTS5 virtual table; calling it inside an aggregate (e.g. MIN(bm25(t)))
/// raises "unable to use function bm25 in the requested context". So we
/// pull (hunk_id, bm25_score) one row per matched symbol, ordered by
/// BM25 ASC, and dedup-by-first-seen in Rust before truncating to k.
pub(crate) fn build_bm25_sql(language: Option<&str>, since_unix: Option<i64>) -> String {
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");
    format!(
        "SELECT h.id, h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message,
                bm25(fts_symbol_name) AS rank_score
         FROM fts_symbol_name
         JOIN symbol sym ON sym.id = fts_symbol_name.symbol_id
         JOIN file_path fp ON fp.id = sym.file_path_id
         JOIN hunk h ON h.file_path_id = fp.id
         JOIN commit_record cr ON cr.sha = h.commit_sha
         WHERE fts_symbol_name MATCH :query
           {ts_filter} {lang_filter}
         ORDER BY rank_score ASC
         LIMIT :k_oversample"
    )
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<HunkHit> {
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
    // SQLite's bm25(<table>) returns negative numbers; -rank_score is positive
    // for a real hit. Map to the `1.0 / (1.0 + (-bm25_raw))` convention so
    // higher = better, matching `knn`'s similarity output.
    let similarity = 1.0 / (1.0 + (-rank_score) as f32);
    Ok(HunkHit {
        hunk_id,
        hunk,
        commit,
        similarity,
    })
}

fn symbol_kind_to_str(k: &SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Const => "const",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #57: the BM25-by-symbol-name SQL **MUST** carry a `LIMIT`
    /// clause. Without it, SQLite's `TEMP B-TREE FOR ORDER BY`
    /// materialises the full hunk-fan-out before Rust's
    /// dedup-by-first-seen ever sees a row (thousands of rows for
    /// hot-file queries). Pinning the literal in this regression
    /// test catches future edits that drop the bound.
    #[test]
    fn bm25_sql_contains_limit_clause() {
        let sql = build_bm25_sql(None, None);
        assert!(
            sql.contains("LIMIT"),
            "BM25-by-symbol-name SQL must include a LIMIT clause to bound \
             the temp B-tree fan-out (issue #57); got:\n{sql}"
        );
    }

    /// Issue #57: the LIMIT must be applied **after** `ORDER BY` so the
    /// rows we keep are the best-scoring ones, not an arbitrary
    /// fan-out prefix. Pin the textual order so a future refactor
    /// can't accidentally swap them.
    #[test]
    fn bm25_sql_orders_before_limit() {
        let sql = build_bm25_sql(None, None);
        let order_idx = sql.find("ORDER BY").expect("SQL contains ORDER BY clause");
        let limit_idx = sql.find("LIMIT").expect("SQL contains LIMIT clause");
        assert!(
            order_idx < limit_idx,
            "ORDER BY must precede LIMIT so SQLite returns the top-scoring \
             rows; got order_idx={order_idx} limit_idx={limit_idx}\nSQL:\n{sql}"
        );
    }
}
