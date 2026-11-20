//! Streaming subprocess runner with deadline kill and transcript tee.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use ohara_core::task::Deadline;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Upper bound on `ProcessExit::stdout_tail`. The transcript stays complete.
pub const STDOUT_TAIL_CAP: usize = 64 * 1024;

/// Grace allowed for the killed child to be reaped after the deadline.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on the helper `kill` invocation, keeping the deadline bound strict.
const KILL_CMD_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to keep collecting queued output after the child is done/killed.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

const READ_CHUNK: usize = 8 * 1024;

/// How a streamed child process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    /// Exit code, if the child exited normally.
    pub code: Option<i32>,
    /// Tail of the combined output, capped at `STDOUT_TAIL_CAP`.
    pub stdout_tail: String,
    /// True if the deadline forced a kill.
    pub killed_at_deadline: bool,
}

/// Run `argv` in `cwd`, teeing stdout and stderr into `transcript`.
///
/// The child runs in its own process group. At the deadline the whole group
/// is sent SIGKILL (`kill -KILL -- -<pgid>`, so forked workers die too, with
/// no `unsafe`), `start_kill` is the backstop for the direct child, and the
/// call returns with `killed_at_deadline: true`. A graceful SIGTERM would need
/// `unsafe`/libc, which the workspace forbids here. Output already in flight
/// is drained (bounded by `DRAIN_GRACE`) before returning, and a child that
/// exits on its own is reported with its real status even if a background
/// grandchild still holds the pipes.
pub async fn run_streaming(
    argv: &[String],
    stdin: Option<String>,
    cwd: &Path,
    deadline: Deadline,
    transcript: &Path,
) -> Result<ProcessExit> {
    run_streaming_env(argv, stdin, cwd, deadline, transcript, None).await
}

/// [`run_streaming`] with an explicit environment: `Some(vars)` clears the
/// inherited environment and passes exactly `vars`; `None` inherits.
pub async fn run_streaming_env(
    argv: &[String],
    stdin: Option<String>,
    cwd: &Path,
    deadline: Deadline,
    transcript: &Path,
    env: Option<&[(String, String)]>,
) -> Result<ProcessExit> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| Error::Runtime("empty command".to_string()))?;
    if let Some(parent) = transcript.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript)
        .await?;

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(vars) = env {
        command.env_clear().envs(vars.iter().map(|(k, v)| (k, v)));
    }
    let mut child = command
        .spawn()
        .map_err(|e| Error::Spawn(format!("{program}: {e}")))?;
    let pid = child.id();

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(pump(out, tx.clone())));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(pump(err, tx.clone())));
    }
    drop(tx);
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            if let Err(e) = pipe.write_all(text.as_bytes()).await {
                tracing::debug!("agent stdin write failed: {e}");
            }
        });
    }

    let sleep = tokio::time::sleep(until(deadline));
    tokio::pin!(sleep);
    let mut tail: Vec<u8> = Vec::new();
    let mut killed = false;
    let mut status = None;
    let mut pipes_open = true;
    let mut failure: Option<Error> = None;
    loop {
        tokio::select! {
            chunk = rx.recv(), if pipes_open => {
                match chunk {
                    Some(chunk) => {
                        if let Err(e) = record(&mut log, &mut tail, &chunk).await {
                            failure = Some(e);
                            break;
                        }
                    }
                    None => pipes_open = false,
                }
            }
            exited = child.wait() => {
                match exited {
                    Ok(st) => status = Some(st),
                    Err(e) => failure = Some(e.into()),
                }
                break;
            }
            _ = &mut sleep => {
                killed = true;
                break;
            }
        }
    }

    if killed || failure.is_some() {
        kill_group(pid).await;
        if let Err(e) = child.start_kill() {
            tracing::warn!("kill at deadline failed: {e}");
        }
    }
    // Collect output still in flight. Bounded, because a surviving
    // grandchild could hold the pipes open indefinitely.
    if failure.is_none() {
        if let Err(e) = drain(&mut rx, &mut log, &mut tail).await {
            kill_group(pid).await;
            failure = Some(e);
        }
    }
    for r in &readers {
        r.abort();
    }
    if let Some(e) = failure {
        return Err(e);
    }
    let status = match status {
        Some(st) => Some(st),
        None => tokio::time::timeout(REAP_GRACE, child.wait())
            .await
            .ok()
            .transpose()?,
    };
    log.flush().await?;
    let code = status.and_then(|st| st.code());
    Ok(ProcessExit {
        code,
        stdout_tail: String::from_utf8_lossy(&tail).into_owned(),
        killed_at_deadline: killed,
    })
}

async fn record(log: &mut tokio::fs::File, tail: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    log.write_all(chunk).await?;
    push_tail(tail, chunk);
    Ok(())
}

async fn drain(
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    log: &mut tokio::fs::File,
    tail: &mut Vec<u8>,
) -> Result<()> {
    let end = tokio::time::Instant::now() + DRAIN_GRACE;
    while let Ok(Some(chunk)) = tokio::time::timeout_at(end, rx.recv()).await {
        record(log, tail, &chunk).await?;
    }
    Ok(())
}

async fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let target = format!("-{pid}");
    let res = tokio::time::timeout(
        KILL_CMD_TIMEOUT,
        Command::new("kill")
            .args(["-KILL", "--", &target])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match res {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("kill of process group {pid} failed: {e}"),
        Err(_) => tracing::warn!("kill of process group {pid} timed out"),
    }
}

fn until(deadline: Deadline) -> Duration {
    (deadline.0 - chrono::Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn push_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() > STDOUT_TAIL_CAP {
        let excess = tail.len() - STDOUT_TAIL_CAP;
        tail.drain(..excess);
    }
}

async fn pump<R: AsyncRead + Unpin>(mut src: R, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn far() -> Deadline {
        Deadline(Utc::now() + chrono::Duration::seconds(60))
    }

    #[tokio::test]
    async fn transcript_contains_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t/transcript.log");
        let exit = run_streaming(
            &sh("echo out-line; echo err-line >&2"),
            None,
            dir.path(),
            far(),
            &log,
        )
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert!(!exit.killed_at_deadline);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("out-line"), "{text}");
        assert!(text.contains("err-line"), "{text}");
        assert!(exit.stdout_tail.contains("out-line"));
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn deadline_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let soon = Deadline(Utc::now() + chrono::Duration::seconds(1));
        let exit = run_streaming(
            &sh(&script),
            None,
            dir.path(),
            soon,
            &dir.path().join("t.log"),
        )
        .await
        .unwrap();
        assert!(exit.killed_at_deadline);
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        let mut gone = false;
        for _ in 0..20 {
            gone = !alive(&pid);
            if gone {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "grandchild {pid} survived the deadline kill");
    }

    #[tokio::test]
    async fn output_just_before_deadline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        let soon = Deadline(Utc::now() + chrono::Duration::seconds(1));
        let exit = run_streaming(
            &sh("echo last-words; exec sleep 60"),
            None,
            dir.path(),
            soon,
            &log,
        )
        .await
        .unwrap();
        assert!(exit.killed_at_deadline);
        assert!(exit.stdout_tail.contains("last-words"));
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .contains("last-words"));
    }

    #[tokio::test]
    async fn normal_exit_with_background_grandchild_is_not_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let exit = run_streaming(
            &sh("sleep 5 & echo finished"),
            None,
            dir.path(),
            far(),
            &dir.path().join("t.log"),
        )
        .await
        .unwrap();
        assert!(!exit.killed_at_deadline);
        assert_eq!(exit.code, Some(0));
        assert!(exit.stdout_tail.contains("finished"));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn closed_pipes_still_hit_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let soon = Deadline(Utc::now() + chrono::Duration::seconds(1));
        let start = std::time::Instant::now();
        let exit = run_streaming(
            &sh("exec >&- 2>&-; sleep 60"),
            None,
            dir.path(),
            soon,
            &dir.path().join("t.log"),
        )
        .await
        .unwrap();
        assert!(exit.killed_at_deadline);
        assert!(
            start.elapsed() < Duration::from_secs(7),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn tail_is_bounded() {
        let mut tail = Vec::new();
        push_tail(&mut tail, &vec![b'a'; STDOUT_TAIL_CAP]);
        push_tail(&mut tail, b"zz");
        assert_eq!(tail.len(), STDOUT_TAIL_CAP);
        assert!(tail.ends_with(b"zz"));
    }
}
