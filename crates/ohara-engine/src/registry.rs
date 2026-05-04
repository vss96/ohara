/// File-locked JSON registry of running `ohara serve` daemons.
///
/// A single JSON file (default `~/.ohara/daemons.json`) stores every live
/// daemon record. Access is serialised via an `fs2` exclusive file-lock so
/// multiple CLI processes can safely read/write concurrently.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("registry I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

// ── Domain types ──────────────────────────────────────────────────────────────

/// A single daemon entry persisted in the registry file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonRecord {
    /// OS process ID of the daemon.
    pub pid: u32,
    /// Absolute path to the Unix-domain socket the daemon listens on.
    pub socket_path: PathBuf,
    /// Cargo package version string (e.g. `"0.7.4"`).
    pub ohara_version: String,
    /// Short git SHA the binary was built from, if embedded at build time.
    pub ohara_git_sha: Option<String>,
    /// `SystemTime` expressed as seconds since the Unix epoch.
    pub started_at_unix: u64,
    /// Seconds since epoch of the most recent successful health ping (0 if
    /// never checked).
    pub last_health_unix: u64,
    /// `true` while the daemon is actively processing a request.
    pub busy: bool,
}

// ── Internal file envelope ────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RegistryFile {
    pub(crate) daemons: Vec<DaemonRecord>,
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Handle to the on-disk daemon registry.
pub struct Registry {
    #[allow(dead_code)]
    path: PathBuf,
}

impl Registry {
    /// Open (or create) the registry at `path`.
    pub fn open(_path: impl AsRef<Path>) -> Result<Self> {
        todo!("D.1: not yet implemented")
    }

    /// Return a snapshot of all registered daemon records.
    pub fn list(&self) -> Result<Vec<DaemonRecord>> {
        todo!("D.1: not yet implemented")
    }

    /// Add or update `rec` in the registry (keyed by `pid`).
    pub fn register(&self, _rec: DaemonRecord) -> Result<()> {
        todo!("D.1: not yet implemented")
    }

    /// Remove the record with the given PID from the registry.
    pub fn unregister(&self, _pid: u32) -> Result<()> {
        todo!("D.1: not yet implemented")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn sample_record(pid: u32) -> DaemonRecord {
        DaemonRecord {
            pid,
            socket_path: PathBuf::from(format!("/tmp/ohara-{pid}.sock")),
            ohara_version: "0.7.4".to_string(),
            ohara_git_sha: Some("abc1234".to_string()),
            started_at_unix: 1_700_000_000,
            last_health_unix: 0,
            busy: false,
        }
    }

    #[test]
    fn round_trip_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemons.json");

        let r1 = Registry::open(&path).unwrap();
        r1.register(sample_record(1001)).unwrap();
        r1.register(sample_record(1002)).unwrap();

        // Open a fresh Registry instance — simulates a second process.
        let r2 = Registry::open(&path).unwrap();
        let records = r2.list().unwrap();

        assert_eq!(records.len(), 2);
        let pids: Vec<u32> = records.iter().map(|r| r.pid).collect();
        assert!(pids.contains(&1001));
        assert!(pids.contains(&1002));
    }

    #[test]
    fn unregister_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemons.json");

        let reg = Registry::open(&path).unwrap();
        reg.register(sample_record(2001)).unwrap();
        reg.register(sample_record(2002)).unwrap();
        reg.unregister(2001).unwrap();

        let records = reg.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 2002);
    }

    #[test]
    fn concurrent_register_does_not_lose_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("daemons.json"));

        // Initialise the file first.
        Registry::open(&*path).unwrap();

        let path_a = Arc::clone(&path);
        let path_b = Arc::clone(&path);

        let t1 = thread::spawn(move || {
            let reg = Registry::open(&*path_a).unwrap();
            reg.register(sample_record(3001)).unwrap();
        });
        let t2 = thread::spawn(move || {
            let reg = Registry::open(&*path_b).unwrap();
            reg.register(sample_record(3002)).unwrap();
        });

        t1.join().expect("thread 1 panicked");
        t2.join().expect("thread 2 panicked");

        let reg = Registry::open(&*path).unwrap();
        let records = reg.list().unwrap();

        assert_eq!(records.len(), 2, "expected 2 records, got: {records:?}");
        let pids: Vec<u32> = records.iter().map(|r| r.pid).collect();
        assert!(pids.contains(&3001));
        assert!(pids.contains(&3002));
    }
}
