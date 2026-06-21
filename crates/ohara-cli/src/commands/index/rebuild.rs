//! `--rebuild` safety + index-file deletion for `ohara index`
//! (issue #90 split). Refuses to delete anything outside `OHARA_HOME`,
//! then removes the index DB and its advisory WAL/SHM sidecars.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Plan 13 Task 3.3 Step 2: refuse `--rebuild` unless the index DB
/// path resolves under `OHARA_HOME`. Defensive belt against an edge
/// case where the path resolver is replaced or `OHARA_HOME` is later
/// altered to point somewhere unexpected.
pub fn assert_rebuild_safe(db_path: &Path, ohara_home: &Path) -> Result<()> {
    if !db_path.starts_with(ohara_home) {
        bail!(
            "refusing to rebuild: index DB path {} is not inside OHARA_HOME {}",
            db_path.display(),
            ohara_home.display(),
        );
    }
    Ok(())
}

/// Plan 13 Task 3.3 Step 1: delete the index DB and its WAL / SHM
/// sidecars. Each remove is best-effort — sidecars may legitimately
/// not exist (a clean shutdown closes the WAL); only a permission /
/// I/O error on the main DB is surfaced.
pub fn delete_index_files(db_path: &Path) -> Result<()> {
    if db_path.exists() {
        std::fs::remove_file(db_path)
            .map_err(|e| anyhow::anyhow!("failed to delete index DB {}: {e}", db_path.display()))?;
    }
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        // Sidecars are advisory; ignore "not found" but surface other I/O.
        if let Err(e) = std::fs::remove_file(&sidecar) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "failed to delete sqlite sidecar {}: {e}",
                    sidecar.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod rebuild_safety_tests {
    use super::*;

    #[test]
    fn assert_rebuild_safe_passes_for_path_under_ohara_home() {
        let home = PathBuf::from("/tmp/some-ohara-home");
        let db = home.join("indexes/abc/index.sqlite");
        assert_rebuild_safe(&db, &home).expect("path under home must be safe");
    }

    #[test]
    fn assert_rebuild_safe_rejects_path_outside_ohara_home() {
        // Defense-in-depth: even if a future resolver returns a path
        // outside OHARA_HOME, --rebuild must refuse rather than
        // silently delete.
        let home = PathBuf::from("/tmp/ohara-home");
        let db = PathBuf::from("/etc/passwd");
        let err = assert_rebuild_safe(&db, &home).expect_err("must reject foreign path");
        assert!(
            err.to_string().contains("not inside OHARA_HOME"),
            "rejection message should name the constraint: {err}"
        );
    }

    #[test]
    fn delete_index_files_removes_main_db_and_present_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("ix.sqlite");
        let wal = dir.path().join("ix.sqlite-wal");
        let shm = dir.path().join("ix.sqlite-shm");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(&wal, b"wal").unwrap();
        std::fs::write(&shm, b"shm").unwrap();
        delete_index_files(&db).expect("delete");
        assert!(!db.exists(), "main db must be gone");
        assert!(!wal.exists(), "wal sidecar must be gone");
        assert!(!shm.exists(), "shm sidecar must be gone");
    }

    #[test]
    fn delete_index_files_is_no_op_when_nothing_to_delete() {
        // Sidecars are advisory — a missing -wal / -shm is not an
        // error. The main DB also being absent is fine for the case
        // where --rebuild runs before any successful index pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("never-existed.sqlite");
        delete_index_files(&db).expect("missing files must be tolerated");
    }
}
