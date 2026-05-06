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
/// of rows on hot files. The SQL aggregates per `hunk.id` (`GROUP BY`)
/// so distinct-hunk selection happens at the SQL level and LIMIT bounds
/// distinct hunks rather than fan-out rows; we still oversample by 10×
/// as defence-in-depth so the optional Rust-side dedup has slack if a
/// future schema change reintroduces duplicates upstream of LIMIT.
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
/// context"), so we compute the per-symbol score in a CTE that targets
/// `fts_symbol_name` directly, then aggregate `MIN(rank_score)` per
/// `hunk.id` in a second CTE. `LIMIT :k_oversample` then bounds
/// **distinct hunks**, not the raw join fan-out — the regression that
/// the LIMIT-only fix in #57 didn't cover.
pub fn bm25_by_name(
    c: &Connection,
    query: &str,
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let sql = build_bm25_sql(language, since_unix);
    // The SQL groups by hunk_id, so LIMIT now bounds distinct hunks.
    // We still oversample by `SYMBOL_LANE_OVERSAMPLE` as defence-in-depth
    // for the Rust-side dedup below. Cast through i64 so
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
    // The CTE's `GROUP BY h.id` already guarantees one row per hunk, so
    // this dedup pass is now defence-in-depth: it survives a future
    // refactor that reintroduces duplicates above LIMIT (e.g. dropping
    // the GROUP BY) without silently violating the one-hunk-per-row
    // contract this lane's callers depend on. Cheap (HashSet over up
    // to k_oversample i64s) so we keep it.
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
/// pin invariants of the query (LIMIT clause, clause ordering, the
/// `PARTITION BY h.id` that guarantees distinct-hunk semantics) without
/// having to construct an FTS5 fixture for every assertion.
///
/// SQLite's `bm25()` carries a hard restriction: it errors with "unable
/// to use function bm25 in the requested context" if its return value
/// is consumed by **any** aggregate function (`MIN`, `SUM`, …) — even
/// across a subquery or CTE boundary. We can't just write
/// `MIN(bm25(...)) GROUP BY hunk_id`. We can, however, expose the
/// per-symbol score through a CTE and reduce-by-hunk with a window
/// function, since the value flows through `ROW_NUMBER()` rather than
/// being aggregated. The pipeline is:
///
///   1. `per_symbol` — `SELECT bm25(fts_symbol_name) ...` directly
///      against the virtual table. One row per matched symbol.
///   2. `per_hunk` — `JOIN hunk h ON h.file_path_id = ...`, expose
///      `ROW_NUMBER() OVER (PARTITION BY h.id ORDER BY rank_score ASC)`.
///      Row 1 of every partition is the best-scoring symbol for that
///      hunk.
///   3. Outer `SELECT ... WHERE rn = 1 ORDER BY rank_score ASC LIMIT
///      :k_oversample` — distinct-by-hunk, sorted, truncated. LIMIT
///      now bounds distinct hunks, not raw fan-out rows, so neither
///      one-symbol-many-hunks nor many-symbols-one-hunk starves the
///      lane of distinct hits up to `k * SYMBOL_LANE_OVERSAMPLE`.
pub(crate) fn build_bm25_sql(language: Option<&str>, since_unix: Option<i64>) -> String {
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");
    format!(
        "WITH per_symbol AS (
             SELECT sym.file_path_id AS file_path_id,
                    bm25(fts_symbol_name) AS rank_score
             FROM fts_symbol_name
             JOIN symbol sym ON sym.id = fts_symbol_name.symbol_id
             WHERE fts_symbol_name MATCH :query
         ),
         per_hunk AS (
             SELECT h.id AS hunk_id,
                    h.commit_sha AS commit_sha,
                    fp.path AS path,
                    fp.language AS language,
                    h.change_kind AS change_kind,
                    h.diff_text AS diff_text,
                    cr.parent_sha AS parent_sha,
                    cr.is_merge AS is_merge,
                    cr.author AS author,
                    cr.ts AS ts,
                    cr.message AS message,
                    per_symbol.rank_score AS rank_score,
                    ROW_NUMBER() OVER (
                        PARTITION BY h.id ORDER BY per_symbol.rank_score ASC
                    ) AS rn
             FROM per_symbol
             JOIN hunk h ON h.file_path_id = per_symbol.file_path_id
             JOIN file_path fp ON fp.id = h.file_path_id
             JOIN commit_record cr ON cr.sha = h.commit_sha
             WHERE 1=1 {ts_filter} {lang_filter}
         )
         SELECT hunk_id, commit_sha, path, language, change_kind, diff_text,
                parent_sha, is_merge, author, ts, message, rank_score
         FROM per_hunk
         WHERE rn = 1
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

    /// Issue #57 follow-up: distinct-hunk selection MUST happen at the
    /// SQL level. `MIN(bm25(...))` is rejected by SQLite ("unable to
    /// use function bm25 in the requested context") so we use a
    /// `ROW_NUMBER() OVER (PARTITION BY h.id ORDER BY rank_score)`
    /// window with a `WHERE rn = 1` filter — equivalent to MIN-by-hunk
    /// without invoking an aggregate over `bm25()`. Without this
    /// distinct-by-SQL step, the LIMIT clause bounds raw join fan-out:
    /// many symbols matching the same hunk consume oversample rows
    /// ahead of genuinely distinct hunks, and the lane silently
    /// returns fewer distinct hits than `k`.
    #[test]
    fn bm25_sql_partitions_distinct_by_hunk_id() {
        let sql = build_bm25_sql(None, None);
        assert!(
            sql.contains("PARTITION BY h.id"),
            "BM25-by-symbol-name SQL must select one row per hunk via \
             ROW_NUMBER() OVER (PARTITION BY h.id ...) so LIMIT bounds \
             distinct hunks (issue #57 follow-up); got:\n{sql}"
        );
        assert!(
            sql.contains("rn = 1"),
            "BM25-by-symbol-name SQL must filter the per-hunk window \
             to rn=1 (the best-scoring symbol for each hunk); got:\n{sql}"
        );
    }
}
