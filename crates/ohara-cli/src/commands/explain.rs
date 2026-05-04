//! `ohara explain <file> [--lines a:b]` — debug subcommand for the Plan 5
//! `explain_change` tool. Prints the same JSON shape the MCP tool emits.
//!
//! Output is a single JSON document (matches `find_pattern`'s
//! pretty-printed style) so the result is pipeable into `jq`.

use anyhow::{anyhow, Result};
use clap::Args as ClapArgs;
use ohara_core::count_lines;
use ohara_core::explain::ExplainQuery;
use ohara_core::perf_trace::timed_phase;
use ohara_engine::client::{find_or_spawn_daemon, registry_path, try_daemon_call};
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::ExplainResult;
use ohara_git::Blamer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Repo-relative path to the file to explain.
    pub file: String,
    /// Line range as `START:END` (1-based, inclusive). Either bound may
    /// be omitted: `:42` → start at line 1; `10:` → end-of-file. If
    /// `--lines` is not supplied at all, the whole file is explained.
    #[arg(long)]
    pub lines: Option<String>,
    /// Number of commits to return (1..=20). Defaults to 5.
    #[arg(short, long, default_value_t = 5)]
    pub k: u8,
    /// Suppress diff excerpts in the output.
    #[arg(long)]
    pub no_diff: bool,
    /// Path to the repo (defaults to current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the explain engine in-process (standalone path).
///
/// Opens storage and a blamer directly, then calls
/// [`ohara_core::explain::explain_change`]. Used both as the direct standalone
/// path and as the fallback when the daemon is unavailable or disabled.
async fn run_standalone(canonical: &Path, q: ExplainQuery) -> Result<ExplainResult> {
    let (repo_id, _, _) = super::resolve_repo_id(canonical)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage =
        Arc::new(timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?);
    let blamer = timed_phase("blamer_open", async { Blamer::open(canonical) }).await?;
    let (hits, meta) =
        ohara_core::explain::explain_change(storage.as_ref(), &blamer, &repo_id, &q).await?;
    Ok(ExplainResult { hits, meta })
}

pub async fn run(args: Args, no_daemon: bool) -> Result<()> {
    let canonical = std::fs::canonicalize(&args.path)
        .map_err(|e| anyhow::anyhow!("canonicalize {}: {e}", args.path.display()))?;

    let (line_start, line_end) = parse_lines(args.lines.as_deref(), &canonical, &args.file)?;
    let explain_query = ExplainQuery {
        file: args.file,
        line_start,
        line_end,
        k: args.k.clamp(1, 20),
        include_diff: !args.no_diff,
        // Plan 12 Task 3.2 default: CLI users get the enrichment so
        // 'why does this code look this way' answers include nearby
        // context too.
        include_related: true,
    };

    let req = Request {
        id: 1,
        repo_path: Some(canonical.to_string_lossy().to_string()),
        method: RequestMethod::ExplainChange(explain_query.clone()),
    };

    let registry = registry_path().map_err(|e| anyhow::anyhow!("registry_path: {e}"))?;
    let current_exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;

    let daemon_resp = try_daemon_call(
        move || {
            find_or_spawn_daemon(
                &current_exe,
                env!("CARGO_PKG_VERSION"),
                option_env!("OHARA_GIT_SHA").unwrap_or("unknown"),
                &registry,
                no_daemon,
            )
        },
        req,
    )
    .await;

    let result: ExplainResult = match daemon_resp {
        Some(resp) if resp.error.is_none() => {
            let value = resp
                .result
                .ok_or_else(|| anyhow::anyhow!("daemon response missing result"))?;
            serde_json::from_value(value)
                .map_err(|e| anyhow::anyhow!("decode ExplainResult: {e}"))?
        }
        _ => run_standalone(&canonical, explain_query).await?,
    };

    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Parse the `--lines` argument. Mirrors the MCP input semantics: a
/// missing or `0`-valued upper bound resolves to the file's actual
/// last line by reading the workdir.
fn parse_lines(spec: Option<&str>, repo_root: &Path, file: &str) -> Result<(u32, u32)> {
    let Some(s) = spec else {
        // No flag: full file. Defer the file-length lookup to here so we
        // only read the file when we actually need it.
        let n = file_line_count(repo_root, file).unwrap_or(0);
        return Ok((1, n.max(1)));
    };
    let (lhs, rhs) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("--lines must be START:END (got {s:?})"))?;
    let start: u32 = if lhs.is_empty() {
        1
    } else {
        lhs.parse()
            .map_err(|e| anyhow!("invalid --lines start {lhs:?}: {e}"))?
    };
    let end: u32 = if rhs.is_empty() {
        let n = file_line_count(repo_root, file).unwrap_or(0);
        n.max(start)
    } else {
        rhs.parse()
            .map_err(|e| anyhow!("invalid --lines end {rhs:?}: {e}"))?
    };
    Ok((start, end))
}

fn file_line_count(repo_root: &Path, file: &str) -> Option<u32> {
    let on_disk = repo_root.join(file);
    let s = std::fs::read_to_string(&on_disk).ok()?;
    Some(count_lines(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lines_full_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "a\nb\nc\n").unwrap();
        let (s, e) = parse_lines(None, dir.path(), "x.rs").unwrap();
        assert_eq!(s, 1);
        assert_eq!(e, 3);
    }

    #[test]
    fn parse_lines_explicit_range() {
        let dir = tempfile::tempdir().unwrap();
        let (s, e) = parse_lines(Some("5:10"), dir.path(), "x.rs").unwrap();
        assert_eq!(s, 5);
        assert_eq!(e, 10);
    }

    #[test]
    fn parse_lines_open_ended_resolves_eof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "a\nb\nc\nd\n").unwrap();
        let (s, e) = parse_lines(Some("2:"), dir.path(), "x.rs").unwrap();
        assert_eq!(s, 2);
        assert_eq!(e, 4);
    }

    #[test]
    fn parse_lines_rejects_missing_colon() {
        let dir = tempfile::tempdir().unwrap();
        let err = parse_lines(Some("42"), dir.path(), "x.rs").unwrap_err();
        assert!(err.to_string().contains("START:END"));
    }
}
