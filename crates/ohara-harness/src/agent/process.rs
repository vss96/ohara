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

const READ_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub stdout_tail: String,
    pub killed_at_deadline: bool,
}

/// Run `argv` in `cwd`, teeing stdout and stderr into `transcript`.
///
/// At the deadline the child is killed (SIGKILL via `start_kill`; a graceful
/// SIGTERM would need `unsafe`/libc, which the workspace forbids here) and the
/// call returns with `killed_at_deadline: true`.
pub async fn run_streaming(
    argv: &[String],
    stdin: Option<String>,
    cwd: &Path,
    deadline: Deadline,
    transcript: &Path,
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

    let mut child = Command::new(program)
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
        .spawn()?;

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
    loop {
        tokio::select! {
            chunk = rx.recv() => {
                match chunk {
                    Some(chunk) => {
                        log.write_all(&chunk).await?;
                        push_tail(&mut tail, &chunk);
                    }
                    None => break,
                }
            }
            _ = &mut sleep => {
                killed = true;
                break;
            }
        }
    }

    if killed {
        if let Err(e) = child.start_kill() {
            tracing::warn!("kill at deadline failed: {e}");
        }
        for r in &readers {
            r.abort();
        }
    }
    let status = match killed {
        true => tokio::time::timeout(REAP_GRACE, child.wait()).await.ok(),
        false => Some(child.wait().await),
    };
    log.flush().await?;
    let code = match status {
        Some(res) => res?.code(),
        None => None,
    };
    Ok(ProcessExit {
        code,
        stdout_tail: String::from_utf8_lossy(&tail).into_owned(),
        killed_at_deadline: killed,
    })
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

    #[test]
    fn tail_is_bounded() {
        let mut tail = Vec::new();
        push_tail(&mut tail, &vec![b'a'; STDOUT_TAIL_CAP]);
        push_tail(&mut tail, b"zz");
        assert_eq!(tail.len(), STDOUT_TAIL_CAP);
        assert!(tail.ends_with(b"zz"));
    }
}
