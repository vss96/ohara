//! `ohara init` — install the post-commit hook (and optionally a CLAUDE.md
//! stanza) so this repo stays auto-indexed.

use anyhow::{anyhow, Context, Result};
use clap::Args as ClapArgs;
use std::fs;
use std::path::{Path, PathBuf};

/// Marker fence opening the ohara-managed block in `.git/hooks/post-commit`.
pub(crate) const HOOK_MARKER_BEGIN: &str = "# >>> ohara managed (do not edit) >>>";
/// Marker fence closing the ohara-managed block.
pub(crate) const HOOK_MARKER_END: &str = "# <<< ohara managed <<<";

/// HTML-comment fence opening the ohara stanza in `CLAUDE.md`.
pub(crate) const CLAUDE_MARKER_BEGIN: &str = "<!-- ohara:start -->";
/// HTML-comment fence closing the ohara stanza in `CLAUDE.md`.
pub(crate) const CLAUDE_MARKER_END: &str = "<!-- ohara:end -->";

/// Body of the managed post-commit hook. Wrapped in markers when written.
pub(crate) const HOOK_BODY: &str =
    "# Re-index this repo on every commit. Silently skipped if `ohara` is not on PATH.
if command -v ohara >/dev/null 2>&1; then
  ( cd \"$(git rev-parse --show-toplevel)\" && ohara index --incremental >/dev/null 2>&1 ) || true
fi";

/// Body of the CLAUDE.md stanza. Wrapped in markers when written.
pub(crate) const CLAUDE_BODY: &str = "## ohara

This repo is indexed by [ohara](https://github.com/vss96/ohara). Use the `find_pattern` MCP tool to ask \"how have we solved X before?\" — it returns ranked commits with diff excerpts and provenance.

- Index updates automatically via the `post-commit` hook installed by `ohara init`.
- Manual refresh: `ohara index --incremental`.
- Status: `ohara status`.";

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Also append/update an "ohara" stanza in CLAUDE.md.
    #[arg(long)]
    pub write_claude_md: bool,
    /// Overwrite an existing post-commit hook even if it lacks the ohara marker.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let repo_root = std::fs::canonicalize(&args.path)
        .with_context(|| format!("canonicalize {}", args.path.display()))?;
    // Locate the .git dir so we honor `git init --separate-git-dir`,
    // worktrees, and submodules. discover() returns the .git directory
    // (or .git file pointer) for whatever repo `path` is inside.
    let repo = git2::Repository::discover(&repo_root).context("discover git repo")?;
    let git_dir = repo.path().to_path_buf();

    let hooks_dir = git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).with_context(|| format!("create {}", hooks_dir.display()))?;
    let hook_path = hooks_dir.join("post-commit");

    write_hook(&hook_path, args.force)?;
    tracing::info!(hook = %hook_path.display(), "wrote post-commit hook");
    println!("installed post-commit hook at {}", hook_path.display());

    if args.write_claude_md {
        let claude_path = repo_root.join("CLAUDE.md");
        write_claude_md(&claude_path)?;
        tracing::info!(claude = %claude_path.display(), "wrote CLAUDE.md stanza");
        println!("updated {}", claude_path.display());
    }

    Ok(())
}

/// Write or update `<repo>/CLAUDE.md`, preserving non-managed content.
///
/// Three cases (per Plan 2 §3), mirroring the hook policy but with HTML
/// comment fences:
///   - File missing → write a fresh CLAUDE.md containing only the stanza.
///   - File present, contains markers → replace stanza in place.
///   - File present, no markers → append the stanza separated by `\n\n`.
fn write_claude_md(path: &Path) -> Result<()> {
    let stanza = format!("{CLAUDE_MARKER_BEGIN}\n{CLAUDE_BODY}\n{CLAUDE_MARKER_END}");

    let new_contents = if !path.exists() {
        format!("{stanza}\n")
    } else {
        let existing =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if existing.contains(CLAUDE_MARKER_BEGIN) && existing.contains(CLAUDE_MARKER_END) {
            replace_block(&existing, CLAUDE_MARKER_BEGIN, CLAUDE_MARKER_END, &stanza)
                .ok_or_else(|| anyhow!("failed to replace ohara stanza in {}", path.display()))?
        } else {
            append_managed_block(&existing, &stanza)
        }
    };

    fs::write(path, new_contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Write or update `.git/hooks/post-commit`, preserving non-managed content.
///
/// Three cases (per Plan 2 §2):
///   - File missing → write a fresh hook (shebang + managed block).
///   - File present, contains markers → replace the block in place.
///   - File present, no markers → append the managed block (separated by
///     a blank line). `--force` overrides this and replaces the whole file.
fn write_hook(path: &Path, force: bool) -> Result<()> {
    let managed = format!("{HOOK_MARKER_BEGIN}\n{HOOK_BODY}\n{HOOK_MARKER_END}");

    let new_contents = if !path.exists() || force {
        format!("#!/bin/sh\n{managed}\n")
    } else {
        let existing =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if existing.contains(HOOK_MARKER_BEGIN) && existing.contains(HOOK_MARKER_END) {
            replace_block(&existing, HOOK_MARKER_BEGIN, HOOK_MARKER_END, &managed)
                .ok_or_else(|| anyhow!("failed to replace managed block in {}", path.display()))?
        } else {
            append_managed_block(&existing, &managed)
        }
    };

    fs::write(path, new_contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    set_executable(path)?;
    Ok(())
}

/// Replace the (single) marker-fenced block in `existing` with `managed`.
/// Returns `None` if either marker is missing or end precedes begin.
fn replace_block(existing: &str, begin: &str, end: &str, managed: &str) -> Option<String> {
    let b = existing.find(begin)?;
    let e_inner = existing[b..].find(end)?;
    let e = b + e_inner + end.len();
    let mut out = String::with_capacity(existing.len() + managed.len());
    out.push_str(&existing[..b]);
    out.push_str(managed);
    out.push_str(&existing[e..]);
    Some(out)
}

/// Append the managed block to existing content, separated by a blank line.
fn append_managed_block(existing: &str, managed: &str) -> String {
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(managed);
    out.push('\n');
    out
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    // Windows has no chmod analog for shell hooks; git for Windows handles
    // execution. Nothing to do.
    Ok(())
}
