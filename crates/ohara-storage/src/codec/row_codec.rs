//! SQL row <-> domain-type codec helpers shared by `hunk`, `symbol`, and
//! `explain` row mappers.
//!
//! These helpers translate between SQLite's text columns and the typed
//! enums defined in `ohara_core::types`. They centralize a single source
//! of truth for the change-kind string spelling and the file-path
//! upsert SQL so the read and write paths can't drift.

use anyhow::{anyhow, Result};
use ohara_core::types::ChangeKind;
use rusqlite::{params, Connection};

/// Stringify a `ChangeKind` for the `hunk.change_kind` column.
pub fn change_kind_to_str(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

/// Parse a `hunk.change_kind` cell back into a `ChangeKind`.
///
/// Returns `Err` for unknown spellings rather than silently collapsing
/// to `Modified` — an unexpected value indicates a corrupt row or a
/// schema drift, which the storage layer should surface as a typed
/// error instead of papering over.
pub fn str_to_change_kind(s: &str) -> Result<ChangeKind> {
    match s {
        "added" => Ok(ChangeKind::Added),
        "modified" => Ok(ChangeKind::Modified),
        "deleted" => Ok(ChangeKind::Deleted),
        "renamed" => Ok(ChangeKind::Renamed),
        other => Err(anyhow!("unknown change_kind: {other}")),
    }
}

/// Insert (or update) a `file_path` row and return its primary key.
///
/// On conflict, preserves any previously-recorded language unless the
/// caller supplies a non-NULL replacement. Used by both the hunk and
/// symbol write paths.
pub fn upsert_file_path(c: &Connection, path: &str, language: Option<&str>) -> Result<i64> {
    c.execute(
        "INSERT INTO file_path (path, language, active) VALUES (?1, ?2, 1)
         ON CONFLICT(path) DO UPDATE SET language = COALESCE(excluded.language, file_path.language)",
        params![path, language],
    )?;
    let id: i64 = c.query_row(
        "SELECT id FROM file_path WHERE path = ?1",
        params![path],
        |r| r.get(0),
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_kind_round_trips_through_string_form() {
        for k in [
            ChangeKind::Added,
            ChangeKind::Modified,
            ChangeKind::Deleted,
            ChangeKind::Renamed,
        ] {
            assert_eq!(str_to_change_kind(change_kind_to_str(k)).unwrap(), k);
        }
    }

    #[test]
    fn str_to_change_kind_returns_err_for_unknown_spelling() {
        let err = str_to_change_kind("squashed").expect_err("unknown spelling must error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown change_kind: squashed"),
            "expected diagnostic naming the bad value, got: {msg}"
        );
    }
}
