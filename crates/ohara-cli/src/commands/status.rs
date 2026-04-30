use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::Storage;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id);
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let st = storage.get_index_status(&repo_id).await?;

    // commits_behind_head is computed by walking from last_indexed_commit to HEAD via git
    let walker = ohara_git::GitWalker::open(&canonical)?;
    let behind = match &st.last_indexed_commit {
        Some(sha) => walker.list_commits(Some(sha))?.len(),
        None => walker.list_commits(None)?.len(),
    };

    println!(
        "repo: {}\nid: {}\nlast_indexed_commit: {}\nindexed_at: {}\ncommits_behind_head: {}",
        canonical.display(),
        repo_id.as_str(),
        st.last_indexed_commit.unwrap_or_else(|| "<none>".into()),
        st.indexed_at.unwrap_or_else(|| "<none>".into()),
        behind
    );
    Ok(())
}
