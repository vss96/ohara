//! `ohara plan` — pre-flight planner that surveys the repo, prints a
//! directory commit-share hotmap, and writes a `.oharaignore` at the
//! repo root.
//!
//! Plan-26 / Spec A. The file lives at the repo root (not `.ohara/`)
//! so it's checked into the repo and shared across the team like
//! `.gitignore`.

#![allow(unused_imports)]

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Write `.oharaignore` without prompting.
    #[arg(long)]
    pub yes: bool,
    /// Print suggestions only; never write a file.
    #[arg(long, conflicts_with = "yes")]
    pub no_write: bool,
    /// Replace the entire `.oharaignore` (default: replace only the
    /// auto-generated section between markers, preserving user lines).
    #[arg(long)]
    pub replace: bool,
}

pub async fn run(_args: Args) -> Result<()> {
    Err(anyhow::anyhow!("plan-26: `ohara plan` not yet implemented"))
}
