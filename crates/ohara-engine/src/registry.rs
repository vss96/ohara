/// File-locked JSON registry of running `ohara serve` daemons.
///
/// A single JSON file (default `~/.ohara/daemons.json`) stores every live
/// daemon record. Access is serialised via an `fs2` exclusive file-lock so
/// multiple CLI processes can safely read/write concurrently.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(unix)]
use libc;

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

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RegistryFile {
    pub(crate) daemons: Vec<DaemonRecord>,
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Handle to the on-disk daemon registry.
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    /// Open (or create) the registry at `path`.
    ///
    /// If the file does not exist its parent directories are created and the
    /// file is initialised with an empty daemon list.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        if !path.exists() {
            let mut f = File::create(&path)?;
            f.write_all(b"{\"daemons\":[]}")?;
        }

        Ok(Self { path })
    }

    /// Return a snapshot of all registered daemon records.
    pub fn list(&self) -> Result<Vec<DaemonRecord>> {
        self.with_locked(|rf| rf.daemons.clone())
    }

    /// Add or update `rec` in the registry (keyed by `pid`).
    ///
    /// If a record with the same PID already exists it is replaced, making
    /// this call idempotent on re-register.
    pub fn register(&self, rec: DaemonRecord) -> Result<()> {
        self.mutate(|rf| {
            rf.daemons.retain(|r| r.pid != rec.pid);
            rf.daemons.push(rec);
            Ok(())
        })
    }

    /// Remove the record with the given PID from the registry.
    pub fn unregister(&self, pid: u32) -> Result<()> {
        self.mutate(|rf| {
            rf.daemons.retain(|r| r.pid != pid);
            Ok(())
        })
    }

    /// Return only records whose PID is still alive and whose last health
    /// check is within the past 5 minutes.  Stale records are pruned from
    /// the registry file in the same operation.
    pub fn list_alive(&self) -> Result<Vec<DaemonRecord>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out = Vec::new();
        for d in self.list()? {
            if !pid_alive(d.pid) {
                continue;
            }
            if now.saturating_sub(d.last_health_unix) > 5 * 60 {
                continue;
            }
            out.push(d);
        }
        let snap = out.clone();
        self.mutate(|f| {
            f.daemons = snap;
            Ok(())
        })?;
        Ok(out)
    }

    /// Return the first alive, non-busy daemon that matches `ohara_version`,
    /// or `None` if no compatible daemon is registered.
    pub fn pick_compatible(&self, ohara_version: &str) -> Result<Option<DaemonRecord>> {
        let alive = self.list_alive()?;
        Ok(alive
            .into_iter()
            .find(|d| d.ohara_version == ohara_version && !d.busy))
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    /// Open the file, acquire an exclusive lock, deserialise, run `f`,
    /// return the result, then unlock.
    fn with_locked<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&RegistryFile) -> T,
    {
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        file.lock_exclusive()?;

        let mut contents = String::new();
        let result = file
            .read_to_string(&mut contents)
            .map_err(RegistryError::from)
            .and_then(|_| serde_json::from_str::<RegistryFile>(&contents).map_err(Into::into))
            .map(|rf| f(&rf));

        // Always unlock — even on error.
        // `fs2::FileExt::unlock` is stable since Rust 1.89; suppress the MSRV
        // lint because we own both sides of this API surface.
        #[allow(clippy::incompatible_msrv)]
        let _ = file.unlock();

        result
    }

    fn mutate<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut RegistryFile) -> Result<()>,
    {
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        file.lock_exclusive()?;

        let result = (|| -> Result<()> {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            let mut rf: RegistryFile = serde_json::from_str(&contents)?;

            f(&mut rf)?;

            let serialised = serde_json::to_vec(&rf)?;
            // Truncate before writing so stale bytes from a longer previous
            // payload are not left at the end of the file.
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&serialised)?;
            file.flush()?;
            Ok(())
        })();

        #[allow(clippy::incompatible_msrv)]
        let _ = file.unlock();

        result
    }
}

// ── OS helpers ────────────────────────────────────────────────────────────────

/// Return `true` if `pid` is a running process visible to this user.
///
/// Uses `kill(pid, 0)` — the POSIX "are you there?" signal — which returns 0
/// if the process exists and we have permission to signal it, or an error
/// otherwise.  This is safe: signal 0 never actually delivers a signal.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is the standard "is process alive?" check.
    // No signal is delivered; the call is used purely for process existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn list_alive_prunes_dead_pids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let r = Registry::open(&path).unwrap();
        // PID 0 is never a valid user process on any Unix; kill(0, 0) sends to
        // the whole process group, so we use a very high PID that is almost
        // certainly not running.
        let dead_pid = 2_000_000_u32;
        r.register(DaemonRecord {
            pid: dead_pid,
            last_health_unix: now_unix(),
            socket_path: PathBuf::from("/tmp/dead.sock"),
            ohara_version: "0.7.4".to_string(),
            ohara_git_sha: None,
            started_at_unix: now_unix(),
            busy: false,
        })
        .unwrap();
        let alive = r.list_alive().unwrap();
        assert!(alive.is_empty(), "dead-pid record must be pruned");
    }

    #[test]
    fn pick_compatible_none_when_only_wrong_version_or_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let r = Registry::open(&path).unwrap();
        let fresh = now_unix();
        let my_pid = std::process::id();

        // Register: alive pid, wrong version, not busy — should NOT be picked for 0.7.4.
        r.register(DaemonRecord {
            pid: my_pid,
            ohara_version: "0.7.3".into(),
            last_health_unix: fresh,
            socket_path: PathBuf::from("/tmp/a.sock"),
            ohara_git_sha: None,
            started_at_unix: fresh,
            busy: false,
        })
        .unwrap();
        assert!(
            r.pick_compatible("0.7.4").unwrap().is_none(),
            "wrong-version daemon must not be picked"
        );
    }

    #[test]
    fn pick_compatible_returns_idle_matching_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let r = Registry::open(&path).unwrap();
        let fresh = now_unix();
        let my_pid = std::process::id();

        // Register: alive pid, correct version, not busy — should be picked.
        r.register(DaemonRecord {
            pid: my_pid,
            ohara_version: "0.7.4".into(),
            last_health_unix: fresh,
            socket_path: PathBuf::from("/tmp/c.sock"),
            ohara_git_sha: None,
            started_at_unix: fresh,
            busy: false,
        })
        .unwrap();
        let pick = r
            .pick_compatible("0.7.4")
            .unwrap()
            .expect("idle 0.7.4 daemon should be picked");
        assert_eq!(pick.pid, my_pid);
        assert!(!pick.busy);
    }

    #[test]
    fn pick_compatible_skips_busy_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let r = Registry::open(&path).unwrap();
        let fresh = now_unix();
        let my_pid = std::process::id();

        // Register: alive pid, correct version, but busy — should NOT be picked.
        r.register(DaemonRecord {
            pid: my_pid,
            ohara_version: "0.7.4".into(),
            last_health_unix: fresh,
            socket_path: PathBuf::from("/tmp/b.sock"),
            ohara_git_sha: None,
            started_at_unix: fresh,
            busy: true,
        })
        .unwrap();
        assert!(
            r.pick_compatible("0.7.4").unwrap().is_none(),
            "busy daemon must not be picked"
        );
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
