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
    // SIGTERM lets it clean up; wait() reaps it so we leave no zombie.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    Err(EngineError::Internal(format!(
        "daemon did not become ready in {readiness_timeout:?}"
    )))
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
        // Writes the pid file (so we can find it) but NEVER the readiness
        // file, then sleeps — modelling a daemon stuck before announcing.
        std::fs::write(
            &script,
            "#!/bin/sh\nshift  # serve\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --pid-file) shift; echo $$ > \"$1\"; shift;;\n    *) shift;;\n  esac\ndone\nsleep 30\n",
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

        // The script wrote its pid; after the spawner returns, that process
        // must no longer be alive (it was SIGTERM'd and reaped).
        let pid_file = std::fs::read_dir(runtime.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "pid"))
            .expect("pid file written by the stuck child");
        let child_pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive_after = unsafe { libc::kill(child_pid, 0) } == 0;
        // Belt-and-suspenders cleanup in case the assertion below would fail.
        unsafe { libc::kill(child_pid, libc::SIGKILL) };
        assert!(
            !alive_after,
            "spawn_daemon must terminate the child on readiness timeout (#79); pid {child_pid} still alive"
        );
    }
}
