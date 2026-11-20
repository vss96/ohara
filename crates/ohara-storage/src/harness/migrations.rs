//! Refinery runner for `harness_migrations/`, embedded separately from the
//! index migration set in `crate::migrations`.

use rusqlite::Connection;

mod embedded {
    refinery::embed_migrations!("harness_migrations");
}

pub fn run(conn: &mut Connection) -> anyhow::Result<()> {
    embedded::migrations::runner().run(conn)?;
    Ok(())
}
