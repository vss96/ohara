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
