//! `ohara update` — self-update the installed `ohara` binary by checking
//! GitHub Releases for a newer version and replacing the on-disk binary
//! in place.
//!
//! Backed by `axoupdater`, the same library that powers cargo-dist's
//! standalone `<app>-update` helper script. Reuses the same source of
//! truth (the release manifest at github.com/vss96/ohara/releases) so
//! `ohara update` and `ohara-cli-update` always agree on what's latest.

use anyhow::{Context, Result};
use axoupdater::AxoUpdater;
use clap::Args as ClapArgs;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Check for an update without installing it. Prints the current and
    /// latest versions and exits non-zero if an update is available.
    #[arg(long)]
    pub check: bool,
    /// Allow downgrades / re-installs of the same version. Off by
    /// default — running `ohara update` on the latest version is a
    /// no-op rather than a re-download.
    #[arg(long)]
    pub force: bool,
    /// Include pre-releases when looking for "latest". Default is
    /// stable-only.
    #[arg(long)]
    pub prerelease: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let mut updater = AxoUpdater::new_for("ohara-cli");
    updater.load_receipt().context(
        "locate the install receipt for ohara-cli (was it installed via the curl|sh installer?)",
    )?;
    if args.prerelease {
        updater.always_update(true);
    }

    if args.check {
        let upgrade = updater
            .is_update_needed()
            .await
            .context("query GitHub Releases for the latest ohara version")?;
        if upgrade {
            let v = updater
                .query_new_version()
                .await
                .context("fetch latest version metadata")?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("update available: {v}");
            std::process::exit(2);
        } else {
            println!("ohara is up to date");
            return Ok(());
        }
    }

    if args.force {
        updater.always_update(true);
    }

    let result = updater
        .run()
        .await
        .context("download and install the latest ohara release")?;
    match result {
        Some(outcome) => {
            println!(
                "updated to {}: installed at {}",
                outcome.new_version, outcome.install_prefix
            );
        }
        None => {
            println!("ohara is up to date");
        }
    }
    Ok(())
}
