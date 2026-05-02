use anyhow::Result;
use rusqlite::Connection;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub fn run(conn: &mut Connection) -> Result<()> {
    embedded::migrations::runner().run(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::pool::{apply_pragmas, load_vec_extension};

    #[test]
    fn migrations_create_hunk_and_vec_hunk_tables_on_fresh_db() {
        crate::codec::pool::register_vec_auto_extension().unwrap(); // register vec extension before opening any connection
        let mut c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();
        run(&mut c).unwrap();

        let count: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let vec_count: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='vec_hunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vec_count, 1);
    }

    #[test]
    fn migrations_v2_creates_fts_tables_and_sibling_names_column() {
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let mut c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();
        run(&mut c).unwrap();

        // V2 must create the two new FTS5 virtual tables.
        let fts_hunk_text: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name='fts_hunk_text'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_hunk_text, 1,
            "V2 should create fts_hunk_text virtual table"
        );

        let fts_symbol_name: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name='fts_symbol_name'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_symbol_name, 1,
            "V2 should create fts_symbol_name virtual table"
        );

        // V2 must add the sibling_names column to symbol with default '[]'.
        let mut found_sibling = false;
        let mut stmt = c.prepare("PRAGMA table_info(symbol)").unwrap();
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let dflt: Option<String> = row.get(4)?;
                let notnull: i64 = row.get(3)?;
                Ok((name, dflt, notnull))
            })
            .unwrap();
        for r in rows {
            let (name, dflt, notnull) = r.unwrap();
            if name == "sibling_names" {
                found_sibling = true;
                assert_eq!(notnull, 1, "sibling_names should be NOT NULL");
                assert_eq!(
                    dflt.as_deref(),
                    Some("'[]'"),
                    "sibling_names default should be '[]'"
                );
            }
        }
        assert!(found_sibling, "symbol.sibling_names column should exist");
    }

    #[test]
    fn migrations_v2_backfills_existing_hunks_and_symbols() {
        // Apply all migrations to a fresh DB; both hunk and symbol seeded
        // before V2 should result in V2-side FTS rows. The cleanest way to
        // exercise the backfill path is to insert V1-shaped rows after V1
        // and before V2 runs. Refinery applies migrations in a single batch,
        // so instead we run V1 manually, seed, then run V2 manually.
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();

        // Apply V1 manually by reading the migration SQL.
        let v1_sql = include_str!("../migrations/V1__initial.sql");
        c.execute_batch(v1_sql).unwrap();

        // Seed file_path + commit_record + hunk + symbol rows under the V1 schema.
        c.execute(
            "INSERT INTO file_path (path, language) VALUES ('a.rs', 'rust')",
            [],
        )
        .unwrap();
        let fp_id: i64 = c
            .query_row("SELECT id FROM file_path WHERE path = 'a.rs'", [], |r| {
                r.get(0)
            })
            .unwrap();
        c.execute(
            "INSERT INTO commit_record (sha, parent_sha, is_merge, ts, author, message)
             VALUES ('c1', NULL, 0, 1, NULL, 'm')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO hunk (commit_sha, file_path_id, change_kind, diff_text)
             VALUES ('c1', ?1, 'added', '+fn foo() {}')",
            [fp_id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO symbol (file_path_id, kind, name, qualified_name,
                                 span_start, span_end, blob_sha, source_text)
             VALUES (?1, 'function', 'foo', NULL, 0, 12, 'sha', 'fn foo() {}')",
            [fp_id],
        )
        .unwrap();

        // Apply V2 manually.
        let v2_sql = include_str!("../migrations/V2__fts_text_and_symbol_name.sql");
        c.execute_batch(v2_sql).unwrap();

        // Backfill assertions.
        let n_text: i64 = c
            .query_row("SELECT count(*) FROM fts_hunk_text", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_text, 1, "fts_hunk_text backfill should have 1 row");
        let n_sym: i64 = c
            .query_row("SELECT count(*) FROM fts_symbol_name", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_sym, 1, "fts_symbol_name backfill should have 1 row");

        // sibling_names defaults to '[]' for v0.2-era rows.
        let sib: String = c
            .query_row("SELECT sibling_names FROM symbol", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sib, "[]");
    }

    #[test]
    fn migrations_v3_creates_index_metadata_table_with_expected_columns() {
        // Plan 13 Task 1.1: V3 adds the index_metadata table that the
        // runtime uses to decide whether a prior index was built with a
        // compatible embedder / chunker / parser version.
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let mut c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();
        run(&mut c).unwrap();

        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name='index_metadata'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "V3 must create the index_metadata table");

        // Column shape: repo_id, component, version, value_json,
        // recorded_at; PK over (repo_id, component).
        let mut stmt = c.prepare("PRAGMA table_info(index_metadata)").unwrap();
        let columns: Vec<(String, String, i64)> = stmt
            .query_map([], |r| {
                let name: String = r.get(1)?;
                let typ: String = r.get(2)?;
                let pk: i64 = r.get(5)?;
                Ok((name, typ, pk))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let names: Vec<&str> = columns.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "repo_id",
                "component",
                "version",
                "value_json",
                "recorded_at",
            ],
            "index_metadata column order is part of the migration contract"
        );
        // PK should cover repo_id (1) + component (2) — sqlite numbers
        // composite PK columns starting at 1.
        let pk_for = |target: &str| -> i64 {
            columns
                .iter()
                .find(|(n, _, _)| n == target)
                .map(|(_, _, pk)| *pk)
                .unwrap_or(0)
        };
        assert_eq!(pk_for("repo_id"), 1, "repo_id is PK part 1");
        assert_eq!(pk_for("component"), 2, "component is PK part 2");
    }

    #[test]
    fn migrations_v4_adds_semantic_text_column_and_hunk_symbol_table() {
        // Plan 11 Task 1.1: V4 introduces hunk.semantic_text, the
        // hunk_symbol attribution table, and the fts_hunk_semantic
        // FTS5 virtual table. This test pins their existence and the
        // attribution-kind contract via column inspection.
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let mut c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();
        run(&mut c).unwrap();

        // hunk.semantic_text — NOT NULL with a '' default so v0.6-era
        // hunks parse cleanly under the new schema.
        let mut sem_found = false;
        let mut stmt = c.prepare("PRAGMA table_info(hunk)").unwrap();
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let typ: String = row.get(2)?;
                let notnull: i64 = row.get(3)?;
                Ok((name, typ, notnull))
            })
            .unwrap();
        for r in rows {
            let (name, typ, notnull) = r.unwrap();
            if name == "semantic_text" {
                sem_found = true;
                assert_eq!(typ, "TEXT");
                assert_eq!(notnull, 1, "semantic_text must be NOT NULL");
            }
        }
        assert!(sem_found, "V4 must add hunk.semantic_text");

        let n_hs: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hunk_symbol'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_hs, 1, "V4 must create hunk_symbol");

        let n_fts: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name='fts_hunk_semantic'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_fts, 1, "V4 must create fts_hunk_semantic");
    }

    #[test]
    fn migrations_v4_backfills_semantic_text_from_diff_text() {
        // Plan 11 Task 1.1 Step 3: existing hunks get
        // semantic_text = diff_text so the new FTS lane returns
        // *something* immediately after migration; no hunk_symbol
        // rows are backfilled (file-level / HEAD-symbol fallback
        // continues to work via the v0.3 lane until a fresh index
        // pass populates the new table).
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();

        // Apply V1+V2+V3 manually so we can seed a v0.6-era hunk.
        for (label, sql) in [
            ("V1", include_str!("../migrations/V1__initial.sql")),
            ("V2", include_str!("../migrations/V2__fts_text_and_symbol_name.sql")),
            ("V3", include_str!("../migrations/V3__index_metadata.sql")),
        ] {
            c.execute_batch(sql)
                .unwrap_or_else(|e| panic!("apply {label}: {e}"));
        }

        c.execute(
            "INSERT INTO file_path (path, language) VALUES ('a.rs', 'rust')",
            [],
        )
        .unwrap();
        let fp_id: i64 = c
            .query_row("SELECT id FROM file_path WHERE path = 'a.rs'", [], |r| {
                r.get(0)
            })
            .unwrap();
        c.execute(
            "INSERT INTO commit_record (sha, parent_sha, is_merge, ts, author, message) \
             VALUES ('c1', NULL, 0, 1, NULL, 'm')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO hunk (commit_sha, file_path_id, change_kind, diff_text) \
             VALUES ('c1', ?1, 'added', '+fn foo() {}')",
            [fp_id],
        )
        .unwrap();

        // Apply V4 manually.
        let v4_sql = include_str!("../migrations/V4__historical_symbol_attribution.sql");
        c.execute_batch(v4_sql).unwrap();

        let semantic: String = c
            .query_row("SELECT semantic_text FROM hunk", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            semantic, "+fn foo() {}",
            "V4 backfill must seed semantic_text = diff_text"
        );
        let fts_n: i64 = c
            .query_row("SELECT count(*) FROM fts_hunk_semantic", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_n, 1, "V4 backfill must mirror existing hunks into the FTS table");
        let hs_n: i64 = c
            .query_row("SELECT count(*) FROM hunk_symbol", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            hs_n, 0,
            "V4 must NOT backfill hunk_symbol — those wait for the next index pass"
        );
    }

    #[test]
    fn migrations_v3_backfills_schema_row_for_existing_repos() {
        // Plan 13 Task 1.1 Step 3: existing repos must get a single
        // `schema = current` row so callers can tell "old index, never
        // recorded any metadata" apart from "freshly migrated; runtime
        // hasn't recorded other components yet".
        crate::codec::pool::register_vec_auto_extension().unwrap();
        let c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();

        // Apply V1 + V2 manually so we can seed a pre-V3 repo row, the
        // same pattern the V2 backfill test uses.
        let v1_sql = include_str!("../migrations/V1__initial.sql");
        c.execute_batch(v1_sql).unwrap();
        let v2_sql = include_str!("../migrations/V2__fts_text_and_symbol_name.sql");
        c.execute_batch(v2_sql).unwrap();

        // Seed two pre-V3 repos.
        c.execute(
            "INSERT INTO repo (id, path, first_commit_sha, schema_version) \
             VALUES ('repo-a', '/tmp/a', 'first-a', 2)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo (id, path, first_commit_sha, schema_version) \
             VALUES ('repo-b', '/tmp/b', 'first-b', 2)",
            [],
        )
        .unwrap();

        // Apply V3 manually.
        let v3_sql = include_str!("../migrations/V3__index_metadata.sql");
        c.execute_batch(v3_sql).unwrap();

        // Each existing repo should have exactly one schema row, and
        // no other component rows.
        let n_total: i64 = c
            .query_row("SELECT count(*) FROM index_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_total, 2,
            "V3 backfill must add one row per existing repo and no others"
        );
        let n_schema: i64 = c
            .query_row(
                "SELECT count(*) FROM index_metadata WHERE component = 'schema'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_schema, 2);
        let version: String = c
            .query_row(
                "SELECT version FROM index_metadata \
                 WHERE repo_id = 'repo-a' AND component = 'schema'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            version, "3",
            "schema backfill records the V3 schema version"
        );
    }
}
