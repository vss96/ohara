use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(short, long)]
    pub query: String,
}

pub async fn run(_args: Args) -> Result<()> {
    anyhow::bail!("ohara query: not yet implemented (Task 16)")
}
