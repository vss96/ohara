use crate::error::EngineError;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub struct SpawnedDaemon {
    pub pid: u32,
    pub socket_path: PathBuf,
}

pub fn spawn_daemon(
    ohara_binary: &Path,
    runtime_dir: &Path,
    ohara_version: &str,
    registry_path: &Path,
) -> crate::Result<SpawnedDaemon> {
    spawn_daemon_inner(
        ohara_binary,
        runtime_dir,
        ohara_version,
        registry_path,
        Duration::from_secs(10),
    )
}

fn spawn_daemon_inner(
    ohara_binary: &Path,
    runtime_dir: &Path,
    ohara_version: &str,
    registry_path: &Path,
    readiness_timeout: Duration,
) -> crate::Result<SpawnedDaemon> {
    std::fs::create_dir_all(runtime_dir)
        .map_err(|e| EngineError::Internal(format!("mkdir runtime: {e}")))?;
    let token = random_8();
    let socket = runtime_dir.join(format!("{ohara_version}-{token}.sock"));
    let pid_file = runtime_dir.join(format!("{ohara_version}-{token}.pid"));
    let ready_file = runtime_dir.join(format!("{ohara_version}-{token}.ready"));

    let mut cmd = Command::new(ohara_binary);
    cmd.arg("serve")
        .arg("--socket")
        .arg(&socket)
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--readiness-file")
        .arg(&ready_file)
        .arg("--registry-path")
        .arg(registry_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    detach_session(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| EngineError::Internal(format!("spawn ohara serve: {e}")))?;

    let started = Instant::now();
    while started.elapsed() < readiness_timeout {
        if ready_file.exists() && pid_file.exists() {
            let pid: u32 = std::fs::read_to_string(&pid_file)
                .map_err(|e| EngineError::Internal(format!("read pid: {e}")))?
                .trim()
                .parse()
                .map_err(|e| EngineError::Internal(format!("parse pid: {e}")))?;
            return Ok(SpawnedDaemon {
                pid,
                socket_path: socket,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Readiness timed out. Terminate the child so it can't finish booting
    // unregistered (find_or_spawn_daemon only registers on success) and
    // hold an undiscoverable socket until its own idle reap (issue #79).
    terminate_and_reap(&mut child);
    // SIGTERM/SIGKILL bypasses the daemon's graceful socket cleanup
    // (serve_unix only removes the socket on a clean shutdown), so
    // best-effort remove anything the half-booted child left behind rather
    // than leaking files in the runtime dir across repeated cold starts.
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&ready_file);
    Err(EngineError::Internal(format!(
        "daemon did not become ready in {readiness_timeout:?}"
    )))
}

/// Terminate a spawned daemon child that never became ready, then reap it
/// within a bounded window. SIGTERM first (a daemon with no handler exits on
/// its default disposition); if it hasn't exited after a short grace period,
/// SIGKILL. The bound matters: `spawn_daemon` runs under the cross-process
/// registry lock, so this must never be an open-ended `wait()`.
fn terminate_and_reap(child: &mut std::process::Child) {
    // SAFETY: `libc::kill` is an FFI call with no memory-safety obligations;
    // `child.id()` is this process's own child pid.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    // Still alive (or try_wait errored): force-kill and reap so the spawner
    // never blocks indefinitely and we leave no zombie.
    let _ = child.kill();
    let _ = child.wait();
}

fn random_8() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:08x}", (nanos as u32) ^ std::process::id())
}

#[cfg(unix)]
fn detach_session(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the forked child before exec.
    // setsid detaches the new process from the controlling terminal
    // and the parent's process group; the parent's wait state is
    // unaffected (the child becomes a session leader).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("ohara");
    }
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let uid = unsafe { libc::geteuid() };
    tmp.join(format!("ohara-{uid}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "spawns a child process; run with --ignored"]
    fn spawn_daemon_writes_pid_and_socket_within_timeout() {
        let runtime = tempfile::tempdir().unwrap();
        let script = runtime.path().join("fake_serve.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nshift  # serve\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --pid-file) shift; echo $$ > \"$1\"; shift;;\n    --readiness-file) shift; printf ready > \"$1\"; shift;;\n    --socket) shift; touch \"$1\"; shift;;\n    *) shift;;\n  esac\ndone\nsleep 30\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = runtime.path().join("registry.json");
        let result =
            spawn_daemon(&script, runtime.path(), "0.7.4", &registry).expect("spawn within 10s");
        // Cleanup: kill the spawned child.
        unsafe { libc::kill(result.pid as i32, libc::SIGTERM) };
        assert!(result.pid > 0);
        assert!(result.socket_path.starts_with(runtime.path()));
    }

    /// Issue #79: a child that never announces readiness (e.g. cold model
    /// cache stalls past the timeout) must be terminated by the spawner —
    /// otherwise it boots unregistered and holds an undiscoverable socket
    /// until its own idle reap (up to 30 min).
    #[test]
    #[ignore = "spawns a child process; run with --ignored"]
    fn spawn_daemon_kills_child_on_readiness_timeout() {
        let runtime = tempfile::tempdir().unwrap();
        let script = runtime.path().join("stuck_serve.sh");
        // `marker` lives outside the spawner-managed file names so the
        // timeout-path cleanup doesn't remove it. The script records its
        // own pid then `exec`s sleep, so the sleeping process IS the spawned
        // pid (no orphaned grandchild). It never writes the readiness file,
        // so the spawner must time out and terminate it.
        let marker = runtime.path().join("child.marker");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = runtime.path().join("registry.json");

        let err = match spawn_daemon_inner(
            &script,
            runtime.path(),
            "0.7.4",
            &registry,
            Duration::from_millis(500),
        ) {
            Ok(_) => panic!("readiness must time out"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("ready"), "got: {err}");

        // The child recorded its pid before exec'ing sleep; after the
        // spawner returns it must have been terminated and reaped.
        let child_pid: i32 = std::fs::read_to_string(&marker)
            .expect("child wrote its pid before exec")
            .trim()
            .parse()
            .unwrap();
        let alive_after = unsafe { libc::kill(child_pid, 0) } == 0;
        // Only signal if still alive — avoids hitting a reused pid once the
        // child has been reaped.
        if alive_after {
            unsafe { libc::kill(child_pid, libc::SIGKILL) };
        }
        assert!(
            !alive_after,
            "spawn_daemon must terminate the child on readiness timeout (#79); pid {child_pid} still alive"
        );
    }
}
