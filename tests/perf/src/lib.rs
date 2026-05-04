//! Shared helpers for the operator-run perf harnesses
//! (`cli_query_bench`, `mcp_query_bench`, `perf_diff`).
//!
//! These are factored out of the individual `[[test]]` binaries so
//! that the workspace-root / fixture-locator / git-sha lookups stay
//! consistent across harnesses.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the workspace root from the perf-tests crate's manifest dir.
///
/// `CARGO_MANIFEST_DIR` points at `tests/perf`; popping two segments
/// reaches the workspace root. Panics on filesystem misconfig because
/// the harness has no useful recovery path.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root resolves")
        .to_path_buf()
}

/// Ensure `fixtures/medium/repo` is built (calling
/// `fixtures/build_medium.sh`) and return its path. Panics on
/// failure — operators want loud failures.
pub fn ensure_medium_fixture() -> PathBuf {
    let root = workspace_root();
    let script = root.join("fixtures/build_medium.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("invoke build_medium.sh");
    assert!(status.success(), "build_medium.sh failed");
    let dest = root.join("fixtures/medium/repo");
    assert!(dest.join(".git").is_dir(), "medium fixture not present");
    dest
}

/// Short git sha of the workspace's current HEAD. Used in run-report
/// filenames so reports for different commits don't collide.
pub fn current_git_sha(root: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Process-lifetime peak resident-set size in bytes.
///
/// macOS: `getrusage(RUSAGE_SELF)` returns `ru_maxrss` in *bytes*
/// (per the Darwin man page; Linux returns kilobytes — we normalise
/// to bytes below). Linux: read `VmHWM` from `/proc/self/status`,
/// which is the high-water-mark RSS in kilobytes. Both APIs are
/// monotonic across the process lifetime, which is exactly what
/// the indexing harness wants ("how much did we use at peak?").
pub fn peak_rss_bytes() -> std::io::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getrusage with RUSAGE_SELF and a stack-allocated
        // rusage is always sound.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ru.ru_maxrss as u64)
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status")?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse().ok())
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("could not parse VmHWM line: {line}"),
                        )
                    })?;
                return Ok(kb * 1024);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "VmHWM not found in /proc/self/status",
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "peak_rss_bytes only implemented for macOS and Linux",
        ))
    }
}

#[cfg(test)]
mod peak_rss_tests {
    use super::peak_rss_bytes;

    #[test]
    fn peak_rss_bytes_returns_nonzero() {
        let n = peak_rss_bytes().expect("rss readable");
        assert!(n > 0, "peak rss must be positive, got {n}");
        // Sanity: any running test process is at least 1 MiB.
        assert!(n > 1024 * 1024, "rss looked too small: {n}");
    }

    #[test]
    fn peak_rss_bytes_grows_after_large_alloc() {
        let before = peak_rss_bytes().unwrap();
        // Touch every page so the OS actually maps it (don't let the
        // optimiser drop the alloc).
        let mut buf: Vec<u8> = vec![0; 64 * 1024 * 1024];
        for i in (0..buf.len()).step_by(4096) {
            buf[i] = (i & 0xff) as u8;
        }
        let after = peak_rss_bytes().unwrap();
        std::hint::black_box(buf);
        assert!(
            after >= before,
            "peak rss must be monotonic across observations: before={before} after={after}"
        );
    }
}
