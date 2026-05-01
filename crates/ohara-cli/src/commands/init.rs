//! `ohara init` — install the post-commit hook (and optionally a CLAUDE.md
//! stanza) so this repo stays auto-indexed.

use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;

// Marker constants are referenced by the implementation in Step 11; they're
// declared here so tests can match against them.

/// Marker fence opening the ohara-managed block in `.git/hooks/post-commit`.
#[allow(dead_code)]
pub(crate) const HOOK_MARKER_BEGIN: &str = "# >>> ohara managed (do not edit) >>>";
/// Marker fence closing the ohara-managed block.
#[allow(dead_code)]
pub(crate) const HOOK_MARKER_END: &str = "# <<< ohara managed <<<";

/// HTML-comment fence opening the ohara stanza in `CLAUDE.md`.
#[allow(dead_code)]
pub(crate) const CLAUDE_MARKER_BEGIN: &str = "<!-- ohara:start -->";
/// HTML-comment fence closing the ohara stanza in `CLAUDE.md`.
#[allow(dead_code)]
pub(crate) const CLAUDE_MARKER_END: &str = "<!-- ohara:end -->";

/// Body of the managed post-commit hook. Wrapped in markers when written.
#[allow(dead_code)]
pub(crate) const HOOK_BODY: &str = "# Re-index this repo on every commit. Silently skipped if `ohara` is not on PATH.
if command -v ohara >/dev/null 2>&1; then
  ( cd \"$(git rev-parse --show-toplevel)\" && ohara index --incremental >/dev/null 2>&1 ) || true
fi";

/// Body of the CLAUDE.md stanza. Wrapped in markers when written.
#[allow(dead_code)]
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

pub async fn run(_args: Args) -> Result<()> {
    unimplemented!("ohara init — implemented in Step 11")
}
