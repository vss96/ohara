use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::query::compute_index_status;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let behind = ohara_git::GitCommitsBehind::open(&canonical)?;
    let st = compute_index_status(storage.as_ref(), &repo_id, &behind).await?;

    println!(
        "repo: {}\nid: {}\nlast_indexed_commit: {}\nindexed_at: {}\ncommits_behind_head: {}",
        canonical.display(),
        repo_id.as_str(),
        st.last_indexed_commit.unwrap_or_else(|| "<none>".into()),
        st.indexed_at.unwrap_or_else(|| "<none>".into()),
        st.commits_behind_head
    );
    Ok(())
}
