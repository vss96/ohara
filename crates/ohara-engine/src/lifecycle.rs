//! Daemon lifecycle owner (issue #95).

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
                started_at_unix: lifecycle_now(),
                last_health_unix: lifecycle_now(),
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
            started_at_unix: lifecycle_now(),
            last_health_unix: lifecycle_now() - 60,
        })
        .unwrap();
        heartbeat(&path, std::process::id());
        let rec = &reg.list().unwrap()[0];
        assert!(rec.last_health_unix >= lifecycle_now() - 1);
    }
}
