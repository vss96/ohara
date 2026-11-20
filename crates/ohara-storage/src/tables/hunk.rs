use anyhow::Result;
use ohara_core::storage::{HunkHit, HunkRecord};
use ohara_core::types::{CommitMeta, Hunk};
use rusqlite::{params, Connection};

use crate::codec::row_codec::{change_kind_to_str, str_to_change_kind, upsert_file_path};
use crate::codec::vec_codec::vec_to_bytes;

/// FTS5 treats characters like `` ` ``, `:`, `(`, `)`, `*`, `^`, `"`,
/// `+`, `-`, etc. as syntax. A user query containing any of them
/// (e.g. `how is \`login\` called`) raises "fts5: syntax error" at
/// query time. Strip everything that isn't a word character, collapse
/// whitespace, and trim — preserves identifier-style search terms
/// without erroring on natural-language phrasing. Shared with the
/// symbol-name BM25 lane via `pub(crate)`.
pub(crate) fn sanitize_fts5_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut last_was_space = true;
    for c in query.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

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
        // Plan 11: same resume-clear pattern for the new
        // fts_hunk_semantic mirror.
        tx.execute(
            "DELETE FROM fts_hunk_semantic WHERE hunk_id IN \
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
        // Plan 11: persist the semantic-text representation alongside
        // the raw diff. Empty semantic_text from a legacy caller falls
        // back to diff_text via COALESCE so the FTS lane never sees
        // an empty document.
        tx.execute(
            "INSERT INTO hunk (commit_sha, file_path_id, change_kind, diff_text, semantic_text)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &r.hunk.commit_sha,
                fp_id,
                change_kind_to_str(r.hunk.change_kind),
                &r.hunk.diff_text,
                if r.semantic_text.is_empty() {
                    &r.hunk.diff_text
                } else {
                    &r.semantic_text
                },
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
        // Plan 11: mirror the semantic_text into fts_hunk_semantic
        // so the new BM25-by-semantic-text lane can query it.
        let semantic_for_fts: &str = if r.semantic_text.is_empty() {
            &r.hunk.diff_text
        } else {
            &r.semantic_text
        };
        tx.execute(
            "INSERT INTO fts_hunk_semantic (hunk_id, content) VALUES (?1, ?2)",
            params![hunk_id, semantic_for_fts],
        )?;
        // Plan 11: persist per-hunk symbol attribution. The hunk
        // delete above (DELETE FROM hunk WHERE commit_sha = ?1)
        // cascades to hunk_symbol via the ON DELETE CASCADE on the
        // foreign key, so re-indexing the same commit doesn't leak
        // stale rows.
        crate::tables::hunk_symbol::put_for_hunk(&tx, hunk_id, &r.symbols)?;
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

/// Plan 11: BM25 over `fts_hunk_semantic`, joined the same way as
/// [`bm25_by_text`]. Targets the normalized hunk-text representation
/// (commit/file/language/symbols/change/added_lines sections) instead
/// of the raw `diff_text`. The two lanes coexist — the retriever
/// fuses them via RRF; the raw-diff lane stays available until the
/// plan-10 evals show the semantic lane is strictly better.
pub fn bm25_by_semantic_text(
    c: &Connection,
    query: &str,
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let lang_filter = language.map(|_| "AND fp.language = :lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= :ts").unwrap_or("");

    let sql = format!(
        "SELECT h.id, h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message,
                bm25(fts_hunk_semantic) AS rank_score
         FROM fts_hunk_semantic f
         JOIN hunk h ON h.id = f.hunk_id
         JOIN file_path fp ON fp.id = h.file_path_id
         JOIN commit_record cr ON cr.sha = h.commit_sha
         WHERE fts_hunk_semantic MATCH :query
           {ts_filter} {lang_filter}
         ORDER BY rank_score ASC
         LIMIT :k"
    );

    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = Vec::new();
    binds.push((":query", Box::new(sanitize_fts5_query(query))));
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
    binds.push((":query", Box::new(sanitize_fts5_query(query))));
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

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::codec::pool::{apply_pragmas, load_vec_extension, register_vec_auto_extension};
    use crate::migrations;
    use crate::tables::{commit, symbol};
    use ohara_core::storage::{CommitRecord, HunkRecord};
    use ohara_core::types::{
        AttributionKind, ChangeKind, CommitMeta, Hunk, HunkSymbol, Symbol, SymbolKind,
    };
    use proptest::prelude::*;

    fn open_migrated_in_memory() -> Connection {
        register_vec_auto_extension().expect("register sqlite-vec");
        let mut c = Connection::open_in_memory().expect("open in-memory sqlite");
        apply_pragmas(&c).expect("apply pragmas");
        load_vec_extension(&c).expect("load vec extension");
        migrations::run(&mut c).expect("run migrations");
        c
    }

    #[derive(Debug, PartialEq)]
    struct CanonicalSnapshot {
        commits: Vec<(String, i64, String, Vec<u8>)>,
        fts_commits: Vec<(String, String)>,
        hunks: Vec<(String, String, String, String, String, Vec<u8>)>,
        fts_text: Vec<(String, String, String)>,
        fts_semantic: Vec<(String, String, String)>,
        hunk_symbols: Vec<(String, String, String, String, String)>,
    }

    fn snapshot_db(c: &Connection) -> CanonicalSnapshot {
        let orphan_vec_hunk: i64 = c
            .query_row(
                "SELECT count(*) FROM vec_hunk WHERE hunk_id NOT IN (SELECT id FROM hunk)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_vec_hunk, 0, "orphaned vec_hunk rows detected");

        let orphan_fts_text: i64 = c
            .query_row(
                "SELECT count(*) FROM fts_hunk_text WHERE hunk_id NOT IN (SELECT id FROM hunk)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_fts_text, 0, "orphaned fts_hunk_text rows detected");

        let orphan_fts_sem: i64 = c
            .query_row(
                "SELECT count(*) FROM fts_hunk_semantic WHERE hunk_id NOT IN (SELECT id FROM hunk)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphan_fts_sem, 0,
            "orphaned fts_hunk_semantic rows detected"
        );

        let orphan_hs: i64 = c
            .query_row(
                "SELECT count(*) FROM hunk_symbol WHERE hunk_id NOT IN (SELECT id FROM hunk)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_hs, 0, "orphaned hunk_symbol rows detected");

        let mut stmt = c
            .prepare(
                "SELECT cr.sha, cr.ts, cr.message, vc.message_emb
                 FROM commit_record cr
                 JOIN vec_commit vc ON vc.commit_sha = cr.sha
                 ORDER BY cr.sha ASC",
            )
            .unwrap();
        let commits = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut stmt = c
            .prepare("SELECT sha, message FROM fts_commit ORDER BY sha ASC, message ASC")
            .unwrap();
        let fts_commits = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut stmt = c
            .prepare(
                "SELECT h.commit_sha, fp.path, h.change_kind, h.diff_text, h.semantic_text, vh.diff_emb
                 FROM hunk h
                 JOIN file_path fp ON fp.id = h.file_path_id
                 JOIN vec_hunk vh ON vh.hunk_id = h.id
                 ORDER BY h.commit_sha ASC, fp.path ASC, h.diff_text ASC",
            )
            .unwrap();
        let hunks = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut stmt = c
            .prepare(
                "SELECT h.commit_sha, fp.path, f.content
                 FROM fts_hunk_text f
                 JOIN hunk h ON h.id = f.hunk_id
                 JOIN file_path fp ON fp.id = h.file_path_id
                 ORDER BY h.commit_sha ASC, fp.path ASC, f.content ASC",
            )
            .unwrap();
        let fts_text = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut stmt = c
            .prepare(
                "SELECT h.commit_sha, fp.path, f.content
                 FROM fts_hunk_semantic f
                 JOIN hunk h ON h.id = f.hunk_id
                 JOIN file_path fp ON fp.id = h.file_path_id
                 ORDER BY h.commit_sha ASC, fp.path ASC, f.content ASC",
            )
            .unwrap();
        let fts_semantic = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut stmt = c
            .prepare(
                "SELECT h.commit_sha, fp.path, hs.symbol_kind, hs.symbol_name, hs.attribution_kind
                 FROM hunk_symbol hs
                 JOIN hunk h ON h.id = hs.hunk_id
                 JOIN file_path fp ON fp.id = h.file_path_id
                 ORDER BY h.commit_sha ASC, fp.path ASC, hs.symbol_kind ASC, hs.symbol_name ASC",
            )
            .unwrap();
        let hunk_symbols = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        CanonicalSnapshot {
            commits,
            fts_commits,
            hunks,
            fts_text,
            fts_semantic,
            hunk_symbols,
        }
    }

    fn is_fts5_reserved(token: &str) -> bool {
        matches!(token, "AND" | "OR" | "NOT" | "NEAR")
    }

    #[test]
    fn bm25_lanes_handle_empty_punctuation_and_operator_queries() {
        // Regression for #118: these queries raised "fts5: syntax error".
        let mut c = open_migrated_in_memory();
        let sha = "b".repeat(40);
        commit::put(
            &mut c,
            &CommitRecord {
                ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                meta: CommitMeta {
                    commit_sha: sha.clone(),
                    parent_sha: None,
                    is_merge: false,
                    author: None,
                    ts: 1_700_000_000,
                    message: "m".into(),
                },
                message_emb: vec![0.1_f32; 384],
            },
        )
        .unwrap();
        put_many(
            &mut c,
            &[HunkRecord {
                hunk: Hunk {
                    commit_sha: sha,
                    file_path: "src/lib.rs".into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Modified,
                    diff_text: "+retry if not ready and near limit or bail\n".into(),
                },
                diff_emb: vec![0.1_f32; 384],
                semantic_text: "retry if not ready and near limit or bail".into(),
                symbols: vec![],
            }],
        )
        .unwrap();

        for q in ["", "   ", "???", "`--`"] {
            assert!(bm25_by_text(&c, q, 5, None, None).unwrap().is_empty());
            assert!(bm25_by_semantic_text(&c, q, 5, None, None)
                .unwrap()
                .is_empty());
            assert!(symbol::bm25_by_name(&c, q, 5, None, None)
                .unwrap()
                .is_empty());
        }
        // Uppercase operators are searched as plain (case-folded) terms.
        for q in ["NOT", "why AND", "retry AND", "foo AND OR bar", "NEAR"] {
            bm25_by_text(&c, q, 5, None, None).unwrap();
            bm25_by_semantic_text(&c, q, 5, None, None).unwrap();
            symbol::bm25_by_name(&c, q, 5, None, None).unwrap();
        }
        assert_eq!(bm25_by_text(&c, "retry AND", 5, None, None).unwrap().len(), 1);
        assert_eq!(bm25_by_text(&c, "NOT", 5, None, None).unwrap().len(), 1);
        assert_eq!(
            bm25_by_semantic_text(&c, "NEAR limit", 5, None, None)
                .unwrap()
                .len(),
            1
        );
        assert!(bm25_by_text(&c, "why AND", 5, None, None)
            .unwrap()
            .is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn sanitize_fts5_query_is_idempotent_and_strips_syntax(q in "\\PC{0,120}") {
            let once = sanitize_fts5_query(&q);
            let twice = sanitize_fts5_query(&once);
            prop_assert_eq!(&once, &twice);
            prop_assert!(!once.starts_with(' '));
            prop_assert!(!once.ends_with(' '));
            prop_assert!(!once.contains("  "));
            for ch in once.chars() {
                prop_assert!(ch.is_alphanumeric() || ch == '_' || ch == ' ');
            }
        }

        #[test]
        fn bm25_lanes_succeed_on_non_empty_non_operator_queries(
            q in "[a-z0-9_ ]{1,50}"
        ) {
            let sanitized = sanitize_fts5_query(&q);
            prop_assume!(!sanitized.is_empty());
            prop_assume!(!sanitized.split_whitespace().any(is_fts5_reserved));

            let mut c = open_migrated_in_memory();
            let sha = "a".repeat(40);
            commit::put(
                &mut c,
                &CommitRecord {
                    ulid: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    meta: CommitMeta {
                        commit_sha: sha.clone(),
                        parent_sha: None,
                        is_merge: false,
                        author: Some("dev".into()),
                        ts: 1_700_000_000,
                        message: "retry backoff handler".into(),
                    },
                    message_emb: vec![0.1_f32; 384],
                },
            )
            .unwrap();
            put_many(
                &mut c,
                &[HunkRecord {
                    hunk: Hunk {
                        commit_sha: sha,
                        file_path: "src/lib.rs".into(),
                        language: Some("rust".into()),
                        change_kind: ChangeKind::Modified,
                        diff_text: "+fn retry_backoff() {}\n".into(),
                    },
                    diff_emb: vec![0.1_f32; 384],
                    semantic_text: "commit: retry\nfile: src/lib.rs\nadded_lines:\nfn retry_backoff() {}".into(),
                    symbols: vec![],
                }],
            )
            .unwrap();
            symbol::put_many(
                &mut c,
                &[Symbol {
                    file_path: "src/lib.rs".into(),
                    language: "rust".into(),
                    kind: SymbolKind::Function,
                    name: "retry_backoff".into(),
                    qualified_name: None,
                    sibling_names: vec![],
                    span_start: 0,
                    span_end: 21,
                    blob_sha: "b1".into(),
                    source_text: "fn retry_backoff() {}".into(),
                }],
            )
            .unwrap();

            prop_assert!(bm25_by_text(&c, &q, 5, None, None).is_ok());
            prop_assert!(bm25_by_semantic_text(&c, &q, 5, None, None).is_ok());
            prop_assert!(symbol::bm25_by_name(&c, &q, 5, None, None).is_ok());
        }

        #[test]
        fn bm25_lanes_never_error_on_arbitrary_user_queries(
            q in prop_oneof![
                Just(String::new()),
                Just("???".to_string()),
                Just("NOT".to_string()),
                Just("why AND".to_string()),
                Just("foo AND OR bar".to_string()),
                "\\PC{0,60}",
            ]
        ) {
            let c = open_migrated_in_memory();
            prop_assert!(
                bm25_by_text(&c, &q, 5, None, None).is_ok(),
                "bm25_by_text errored on query {q:?}"
            );
            prop_assert!(
                bm25_by_semantic_text(&c, &q, 5, None, None).is_ok(),
                "bm25_by_semantic_text errored on query {q:?}"
            );
            prop_assert!(
                symbol::bm25_by_name(&c, &q, 5, None, None).is_ok(),
                "symbol::bm25_by_name errored on query {q:?}"
            );
        }

        #[test]
        fn crash_resume_replay_is_idempotent_across_relational_fts_and_vec_tables(
            ops in proptest::collection::vec(
                (0u8..=3, 1i64..=10_000, "[a-z]{3,12}", 1usize..=3),
                1..=10
            )
        ) {
            let mut replay_db = open_migrated_in_memory();
            let mut dedup_ops: std::collections::BTreeMap<u8, (i64, String, usize)> =
                std::collections::BTreeMap::new();

            let write_op = |conn: &mut Connection, slot: u8, ts: i64, word: &str, n_hunks: usize| {
                let sha = format!("{:040x}", slot + 1);
                let rec = CommitRecord {
                    ulid: format!("01ARZ3NDEKTSV4RRFFQ69G5FA{slot}"),
                    meta: CommitMeta {
                        commit_sha: sha.clone(),
                        parent_sha: None,
                        is_merge: false,
                        author: Some("alice".into()),
                        ts,
                        message: format!("msg {word} {ts}"),
                    },
                    message_emb: vec![slot as f32 + 0.25; 384],
                };
                commit::put(conn, &rec).unwrap();

                let hunks: Vec<HunkRecord> = (0..n_hunks)
                    .map(|idx| HunkRecord {
                        hunk: Hunk {
                            commit_sha: sha.clone(),
                            file_path: format!("src/f_{idx}.rs"),
                            language: Some("rust".into()),
                            change_kind: ChangeKind::Modified,
                            diff_text: format!("+fn {word}_{idx}() {{}}\n"),
                        },
                        diff_emb: vec![(idx + 1) as f32 * 0.1; 384],
                        semantic_text: format!("commit: msg {word}\nfile: src/f_{idx}.rs"),
                        symbols: vec![HunkSymbol {
                            kind: SymbolKind::Function,
                            name: format!("{word}_{idx}"),
                            qualified_name: None,
                            attribution: AttributionKind::ExactSpan,
                        }],
                    })
                    .collect();
                put_many(conn, &hunks).unwrap();
            };

            for (slot, ts, word, n_hunks) in ops {
                write_op(&mut replay_db, slot, ts, &word, n_hunks);
                dedup_ops.insert(slot, (ts, word, n_hunks));
            }

            let mut fresh_db = open_migrated_in_memory();
            for (slot, (ts, word, n_hunks)) in dedup_ops {
                write_op(&mut fresh_db, slot, ts, &word, n_hunks);
            }

            prop_assert_eq!(snapshot_db(&replay_db), snapshot_db(&fresh_db));
        }
    }
}
