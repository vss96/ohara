//! Daemon lifecycle owner (issue #95).
//!
//! One module owns the registry-facing lifecycle of an `ohara serve`
//! daemon; [`crate::registry`] is reduced to persistence.
//!
//! Phases, all on the client side under the registry's exclusive lock:
//! 1. **Pick** — prune dead / stale records, reuse a live record whose
//!    `ohara_version` matches.
//! 2. **Launch** — otherwise ask the [`DaemonLauncher`] to start one; the
//!    production [`ProcessLauncher`] fork/execs `ohara serve`, waits for
//!    its readiness file, and reaps a child that misses the deadline.
//! 3. **Register** — only a successfully launched daemon is recorded,
//!    before the lock is released, so no other client can observe a
//!    ready-but-unregistered socket (the #79 window).
//!
//! On the daemon side, [`heartbeat`] keeps the record's
//! `last_health_unix` fresh so the pick phase does not prune it.

use crate::client::spawn::{runtime_dir, spawn_daemon, SpawnedDaemon};
use crate::client::DaemonHandle;
use crate::error::EngineError;
use crate::registry::{DaemonRecord, Registry};
use std::path::{Path, PathBuf};

/// Starts a daemon process and waits until it is ready to serve. The
/// test seam for [`DaemonLifecycle`]: production uses [`ProcessLauncher`].
pub trait DaemonLauncher {
    fn launch(&self) -> crate::Result<SpawnedDaemon>;
}

impl<T: DaemonLauncher + ?Sized> DaemonLauncher for &T {
    fn launch(&self) -> crate::Result<SpawnedDaemon> {
        (**self).launch()
    }
}

/// Launches `ohara serve` as a detached child process (see
/// [`spawn_daemon`]).
pub struct ProcessLauncher {
    pub ohara_binary: PathBuf,
    pub ohara_version: String,
    pub registry_path: PathBuf,
}

impl DaemonLauncher for ProcessLauncher {
    fn launch(&self) -> crate::Result<SpawnedDaemon> {
        spawn_daemon(
            &self.ohara_binary,
            &runtime_dir(),
            &self.ohara_version,
            &self.registry_path,
        )
    }
}

/// Acquire-side owner of the daemon lifecycle for one `ohara` version.
pub struct DaemonLifecycle<L: DaemonLauncher> {
    registry: Registry,
    ohara_version: String,
    ohara_git_sha: String,
    launcher: L,
}

impl<L: DaemonLauncher> DaemonLifecycle<L> {
    pub fn new(
        registry: Registry,
        ohara_version: impl Into<String>,
        ohara_git_sha: impl Into<String>,
        launcher: L,
    ) -> Self {
        Self {
            registry,
            ohara_version: ohara_version.into(),
            ohara_git_sha: ohara_git_sha.into(),
            launcher,
        }
    }

    /// Return a live same-version daemon, launching and registering one
    /// if none exists. Atomic with respect to other clients: the whole
    /// pick → launch → register sequence runs under the registry lock, and
    /// a failed launch registers nothing.
    pub fn acquire(&self) -> crate::Result<DaemonHandle> {
        self.registry
            .locked_update(|daemons| self.pick_or_launch(daemons))
            .map_err(|e| EngineError::Internal(format!("registry locked_update: {e}")))?
    }

    fn pick_or_launch(&self, daemons: &mut Vec<DaemonRecord>) -> crate::Result<DaemonHandle> {
        if let Some(existing) = daemons
            .iter()
            .find(|d| d.ohara_version == self.ohara_version)
        {
            return Ok(DaemonHandle {
                socket_path: existing.socket_path.clone(),
                pid: existing.pid,
                spawned: false,
            });
        }
        let launched = self
            .launcher
            .launch()
            .map_err(|e| EngineError::Internal(format!("spawn daemon: {e}")))?;
        let now = now_unix();
        daemons.push(DaemonRecord {
            pid: launched.pid,
            socket_path: launched.socket_path.clone(),
            ohara_version: self.ohara_version.clone(),
            ohara_git_sha: Some(self.ohara_git_sha.clone()),
            started_at_unix: now,
            last_health_unix: now,
        });
        Ok(DaemonHandle {
            socket_path: launched.socket_path,
            pid: launched.pid,
            spawned: true,
        })
    }
}

/// Daemon-side liveness signal: refresh `last_health_unix` for `pid` in
/// the registry at `registry_path`. Best-effort — a registry that can't be
/// opened or a pid that isn't (yet) registered is logged at debug level
/// and otherwise ignored; the next tick retries.
pub fn heartbeat(registry_path: &Path, pid: u32) {
    let touched = Registry::open(registry_path).and_then(|reg| reg.touch_health(pid));
    if let Err(e) = touched {
        tracing::debug!(error = %e, pid, "daemon heartbeat failed");
    }
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::client::SpawnedDaemon;
    use crate::registry::{DaemonRecord, Registry};
    use std::cell::Cell;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Fake launcher: counts launches; "spawns" a daemon whose pid is this
    /// test process (alive, so the registry's dead-pid prune keeps it).
    struct FakeLauncher {
        launches: Cell<u32>,
        fail: bool,
    }

    impl FakeLauncher {
        fn ok() -> Self {
            Self {
                launches: Cell::new(0),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                launches: Cell::new(0),
                fail: true,
            }
        }
    }

    impl DaemonLauncher for FakeLauncher {
        fn launch(&self) -> crate::Result<SpawnedDaemon> {
            self.launches.set(self.launches.get() + 1);
            if self.fail {
                return Err(crate::EngineError::Internal("boom".into()));
            }
            Ok(SpawnedDaemon {
                pid: std::process::id(),
                socket_path: PathBuf::from(format!("/tmp/fake-{}.sock", self.launches.get())),
            })
        }
    }

    fn registry(dir: &tempfile::TempDir) -> Registry {
        Registry::open(dir.path().join("registry.json")).unwrap()
    }

    #[test]
    fn acquire_launches_and_registers_when_registry_is_empty() {
        let dir = tempdir().unwrap();
        let launcher = FakeLauncher::ok();
        let lc = DaemonLifecycle::new(registry(&dir), "1.2.3", "sha", &launcher);
        let h = lc.acquire().unwrap();
        assert!(h.spawned);
        assert_eq!(h.pid, std::process::id());
        assert_eq!(launcher.launches.get(), 1);
        let recs = registry(&dir).list().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].ohara_version, "1.2.3");
        assert_eq!(recs[0].ohara_git_sha.as_deref(), Some("sha"));
        assert_eq!(recs[0].socket_path, h.socket_path);
    }

    #[test]
    fn acquire_reuses_a_live_same_version_daemon_without_launching() {
        let dir = tempdir().unwrap();
        let launcher = FakeLauncher::ok();
        let lc = DaemonLifecycle::new(registry(&dir), "1.2.3", "sha", &launcher);
        let first = lc.acquire().unwrap();
        let second = lc.acquire().unwrap();
        assert!(!second.spawned);
        assert_eq!(second.socket_path, first.socket_path);
        assert_eq!(launcher.launches.get(), 1, "second acquire must not launch");
    }

    #[test]
    fn acquire_launches_when_only_other_versions_are_registered() {
        let dir = tempdir().unwrap();
        registry(&dir)
            .register(DaemonRecord {
                pid: std::process::id(),
                socket_path: PathBuf::from("/tmp/old.sock"),
                ohara_version: "0.0.1".into(),
                ohara_git_sha: None,
                started_at_unix: now_unix(),
                last_health_unix: now_unix(),
            })
            .unwrap();
        let launcher = FakeLauncher::ok();
        let lc = DaemonLifecycle::new(registry(&dir), "1.2.3", "sha", &launcher);
        let h = lc.acquire().unwrap();
        assert!(h.spawned);
        assert_eq!(launcher.launches.get(), 1);
        assert_eq!(registry(&dir).list().unwrap().len(), 2);
    }

    #[test]
    fn failed_launch_registers_nothing() {
        let dir = tempdir().unwrap();
        let launcher = FakeLauncher::failing();
        let lc = DaemonLifecycle::new(registry(&dir), "1.2.3", "sha", &launcher);
        assert!(lc.acquire().is_err());
        assert!(registry(&dir).list().unwrap().is_empty());
    }

    #[test]
    fn heartbeat_advances_last_health_of_the_registered_daemon() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let reg = Registry::open(&path).unwrap();
        reg.register(DaemonRecord {
            pid: std::process::id(),
            socket_path: PathBuf::from("/tmp/x.sock"),
            ohara_version: "1.2.3".into(),
            ohara_git_sha: None,
            started_at_unix: now_unix(),
            last_health_unix: now_unix() - 60,
        })
        .unwrap();
        heartbeat(&path, std::process::id());
        let rec = &reg.list().unwrap()[0];
        assert!(rec.last_health_unix >= now_unix() - 1);
    }
}
