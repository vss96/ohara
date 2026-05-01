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
    use crate::pool::{apply_pragmas, load_vec_extension};

    #[test]
    fn migrations_apply_to_fresh_db() {
        crate::pool::register_vec_auto_extension().unwrap(); // register vec extension before opening any connection
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
        crate::pool::register_vec_auto_extension().unwrap();
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
        crate::pool::register_vec_auto_extension().unwrap();
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
            .query_row(
                "SELECT id FROM file_path WHERE path = 'a.rs'",
                [],
                |r| r.get(0),
            )
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
}
