use crate::client::spawn::{runtime_dir, spawn_daemon};
use crate::error::EngineError;
use crate::registry::{DaemonRecord, Registry};
use std::path::{Path, PathBuf};

/// A live or newly-spawned daemon the client can connect to.
pub struct DaemonHandle {
    pub socket_path: PathBuf,
    pub pid: u32,
    /// `true` when this process spawned the daemon, `false` when an existing
    /// compatible daemon was reused.
    pub spawned: bool,
}

/// Locate a compatible running daemon or spawn a fresh one.
///
/// Returns `Ok(None)` when daemon use is disabled either by the caller
/// (`no_daemon = true`) or by the environment (`CI=true` and
/// `OHARA_FORCE_DAEMON` is not set).
pub fn find_or_spawn_daemon(
    _ohara_binary: &Path,
    _ohara_version: &str,
    _ohara_git_sha: &str,
    _registry_path: &Path,
    _no_daemon: bool,
) -> crate::Result<Option<DaemonHandle>> {
    todo!("D.6: implement find_or_spawn_daemon")
}

/// Return the platform-appropriate path for the daemon registry file.
///
/// - macOS: `~/Library/Caches/ohara/daemon/<version>/registry.json`
/// - Linux/other: `${XDG_CACHE_HOME:-~/.cache}/ohara/daemon/<version>/registry.json`
pub fn registry_path() -> crate::Result<PathBuf> {
    todo!("D.6: implement registry_path")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::tempdir;

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn no_daemon_flag_returns_none() {
        let dir = tempdir().unwrap();
        let reg = dir.path().join("registry.json");
        let h = find_or_spawn_daemon(
            Path::new("/nonexistent"),
            "0.7.4",
            "abc",
            &reg,
            true, // no_daemon
        )
        .expect("ok");
        assert!(h.is_none());
    }

    #[test]
    fn ci_env_returns_none_unless_force() {
        let _g = env_lock();
        let dir = tempdir().unwrap();
        let reg = dir.path().join("registry.json");
        let prev_ci = std::env::var_os("CI");
        let prev_force = std::env::var_os("OHARA_FORCE_DAEMON");
        std::env::set_var("CI", "true");
        std::env::remove_var("OHARA_FORCE_DAEMON");
        let h = find_or_spawn_daemon(Path::new("/nonexistent"), "0.7.4", "abc", &reg, false)
            .expect("ok");
        assert!(
            h.is_none(),
            "CI=true must disable daemon spawn unless overridden"
        );
        match prev_ci {
            Some(v) => std::env::set_var("CI", v),
            None => std::env::remove_var("CI"),
        }
        if let Some(v) = prev_force {
            std::env::set_var("OHARA_FORCE_DAEMON", v);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn registry_path_uses_xdg_cache_home_when_set() {
        let _g = env_lock();
        let prev = std::env::var_os("XDG_CACHE_HOME");
        let dir = tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let p = registry_path().expect("path");
        assert!(p.starts_with(dir.path().join("ohara/daemon")), "got {p:?}");
        match prev {
            Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }
}
