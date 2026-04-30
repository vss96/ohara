use anyhow::{Context, Result};
use ohara_core::types::RepoId;
use ohara_core::{EmbeddingProvider, Retriever, Storage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct OharaServer {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,
    pub storage: Arc<dyn Storage>,
    /// Kept alive so the FastEmbed model held inside `retriever` stays loaded
    /// for the lifetime of the server. Not read directly.
    #[allow(dead_code)]
    pub embedder: Arc<dyn EmbeddingProvider>,
    pub retriever: Retriever,
}

impl OharaServer {
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        let walker = ohara_git::GitWalker::open(&canonical).context("open repo")?;
        let first_commit = walker.first_commit_sha()?;
        let repo_id = RepoId::from_parts(&first_commit, &canonical.to_string_lossy());

        let home = std::env::var("OHARA_HOME").map(PathBuf::from).unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).expect("HOME"))
                .join(".ohara")
        });
        let db_path = home.join(repo_id.as_str()).join("index.sqlite");

        let storage: Arc<dyn Storage> = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(
            tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new).await??
        );
        let retriever = Retriever::new(storage.clone(), embedder.clone());

        Ok(Self { repo_id, repo_path: canonical, storage, embedder, retriever })
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }

    pub async fn index_status_meta(&self) -> Result<ohara_core::query::ResponseMeta> {
        let st = self.storage.get_index_status(&self.repo_id).await?;
        let walker = ohara_git::GitWalker::open(&self.repo_path)?;
        let behind = match &st.last_indexed_commit {
            Some(sha) => walker.list_commits(Some(sha))?.len() as u64,
            None => walker.list_commits(None)?.len() as u64,
        };
        let hint = if st.last_indexed_commit.is_none() {
            Some("Index not built. Run `ohara index` in this repo.".to_string())
        } else if behind > 50 {
            Some(format!("Index is {behind} commits behind HEAD. Run `ohara index`."))
        } else { None };
        Ok(ohara_core::query::ResponseMeta {
            index_status: ohara_core::query::IndexStatus {
                last_indexed_commit: st.last_indexed_commit,
                commits_behind_head: behind,
                indexed_at: st.indexed_at,
            },
            hint,
        })
    }
}
