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
}
