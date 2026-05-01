//! Filesystem path helpers shared by the CLI and MCP server.
//!
//! The single source of truth for `OHARA_HOME` resolution and per-repo
//! index database paths. CLI and MCP both call these so the layout stays
//! consistent.

use crate::types::RepoId;
use crate::Result;
use std::path::PathBuf;

/// Resolve the on-disk root for ohara state.
///
/// Honors `$OHARA_HOME` if set; otherwise falls back to `$HOME/.ohara`
/// (or `$USERPROFILE/.ohara` on Windows). Returns `OhraError::Config`
/// if no suitable home directory is set.
pub fn ohara_home() -> Result<PathBuf> {
    unimplemented!("ohara_home — implemented in Step 5")
}

/// Per-repo SQLite index database path: `<ohara_home>/<repo_id>/index.sqlite`.
pub fn index_db_path(_id: &RepoId) -> Result<PathBuf> {
    unimplemented!("index_db_path — implemented in Step 5")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this module mutate process-global env vars; serialize them
    /// behind a Mutex so they don't race when run concurrently.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ohara_home_uses_env_when_set() {
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", "/tmp/ohara-test-home");
        let p = ohara_home().expect("ohara_home should succeed when OHARA_HOME set");
        assert_eq!(p, PathBuf::from("/tmp/ohara-test-home"));
        std::env::remove_var("OHARA_HOME");
    }

    #[test]
    fn ohara_home_falls_back_to_home() {
        let _g = env_lock();
        std::env::remove_var("OHARA_HOME");
        std::env::set_var("HOME", "/tmp/fake-home");
        let p = ohara_home().expect("ohara_home should fall back to HOME");
        assert_eq!(p, PathBuf::from("/tmp/fake-home/.ohara"));
    }

    #[test]
    fn index_db_path_joins_repo_id_and_filename() {
        let _g = env_lock();
        std::env::set_var("OHARA_HOME", "/tmp/ohara-idx-test");
        let id = RepoId::from_parts("deadbeef", "/Users/x/foo");
        let p = index_db_path(&id).expect("index_db_path should succeed");
        assert_eq!(p, PathBuf::from(format!("/tmp/ohara-idx-test/{}/index.sqlite", id.as_str())));
        std::env::remove_var("OHARA_HOME");
    }
}
