use crate::client::spawn::{runtime_dir, spawn_daemon};
use crate::error::EngineError;
use crate::registry::{DaemonRecord, Registry};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use libc;

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
    ohara_binary: &Path,
    ohara_version: &str,
    ohara_git_sha: &str,
    registry_path: &Path,
    no_daemon: bool,
) -> crate::Result<Option<DaemonHandle>> {
    if no_daemon {
        return Ok(None);
    }
    let in_ci = std::env::var("CI")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false);
    if in_ci && std::env::var_os("OHARA_FORCE_DAEMON").is_none() {
        return Ok(None);
    }
    let reg = Registry::open(registry_path)
        .map_err(|e| EngineError::Internal(format!("registry open: {e}")))?;
    if let Some(existing) = reg
        .pick_compatible(ohara_version)
        .map_err(|e| EngineError::Internal(format!("registry pick: {e}")))?
    {
        return Ok(Some(DaemonHandle {
            socket_path: existing.socket_path.clone(),
            pid: existing.pid,
            spawned: false,
        }));
    }
    let sd = spawn_daemon(ohara_binary, &runtime_dir(), ohara_version)?;
    let record = DaemonRecord {
        pid: sd.pid,
        socket_path: sd.socket_path.clone(),
        ohara_version: ohara_version.into(),
        ohara_git_sha: Some(ohara_git_sha.into()),
        started_at_unix: now_unix(),
        last_health_unix: now_unix(),
        busy: false,
    };
    if let Err(register_err) = reg.register(record) {
        tracing::warn!(
            pid = sd.pid,
            error = %register_err,
            "register failed; killing orphan daemon"
        );
        #[cfg(unix)]
        // SAFETY: SIGTERM is delivered to the child we just spawned.
        // The pid is fresh from spawn_daemon and has not been reused.
        unsafe {
            libc::kill(sd.pid as libc::pid_t, libc::SIGTERM);
        }
        return Err(EngineError::Internal(format!(
            "registry register: {register_err}"
        )));
    }
    Ok(Some(DaemonHandle {
        socket_path: sd.socket_path,
        pid: sd.pid,
        spawned: true,
    }))
}

/// Return the platform-appropriate path for the daemon registry file.
///
/// - macOS: `~/Library/Caches/ohara/daemon/<version>/registry.json`
/// - Linux/other: `${XDG_CACHE_HOME:-~/.cache}/ohara/daemon/<version>/registry.json`
pub fn registry_path() -> crate::Result<PathBuf> {
    let base = registry_base()?;
    Ok(base
        .join("daemon")
        .join(env!("CARGO_PKG_VERSION"))
        .join("registry.json"))
}

fn registry_base() -> crate::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home =
            std::env::var_os("HOME").ok_or_else(|| EngineError::Internal("HOME not set".into()))?;
        return Ok(PathBuf::from(home).join("Library/Caches/ohara"));
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(d).join("ohara"));
    }
    let home =
        std::env::var_os("HOME").ok_or_else(|| EngineError::Internal("HOME not set".into()))?;
    Ok(PathBuf::from(home).join(".cache/ohara"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
