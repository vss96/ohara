use anyhow::{Context, Result};
use ohara_core::embed::RerankProvider;
use ohara_core::types::RepoId;
use ohara_core::{EmbeddingProvider, Retriever, Storage};
use ohara_git::Blamer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct OharaServer {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,
    pub storage: Arc<dyn Storage>,
    pub retriever: Retriever,
    /// Plan 5: blame source backing the `explain_change` tool. One per
    /// session; reuses the underlying `git2::Repository` via
    /// `Arc<Mutex<Repository>>` (set up inside `Blamer::open`).
    pub blamer: Arc<Blamer>,
}

impl OharaServer {
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        let walker = ohara_git::GitWalker::open(&canonical).context("open repo")?;
        let first_commit = walker.first_commit_sha()?;
        let repo_id = RepoId::from_parts(&first_commit, &canonical.to_string_lossy());

        let db_path = ohara_core::paths::index_db_path(&repo_id)?;

        let storage: Arc<dyn Storage> =
            Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::new(tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new).await??);
        // Plan 3: attach the cross-encoder reranker by default. Per-call
        // opt-out is the MCP `no_rerank: true` flag, plumbed through
        // `PatternQuery`. First boot downloads ~110 MB for bge-reranker-base.
        let reranker: Arc<dyn RerankProvider> =
            Arc::new(tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new).await??);
        let retriever = Retriever::new(storage.clone(), embedder.clone()).with_reranker(reranker);

        // Plan 5: blame source for `explain_change`. Reads from the same
        // workdir; no model download or async work needed.
        let blamer = Arc::new(Blamer::open(&canonical).context("open blamer")?);

        Ok(Self {
            repo_id,
            repo_path: canonical,
            storage,
            retriever,
            blamer,
        })
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }

    pub async fn index_status_meta(&self) -> Result<ohara_core::query::ResponseMeta> {
        let behind = ohara_git::GitCommitsBehind::open(&self.repo_path)?;
        let st =
            ohara_core::query::compute_index_status(self.storage.as_ref(), &self.repo_id, &behind)
                .await?;
        let hint = if st.last_indexed_commit.is_none() {
            Some("Index not built. Run `ohara index` in this repo.".to_string())
        } else if st.commits_behind_head > 50 {
            Some(format!(
                "Index is {} commits behind HEAD. Run `ohara index`.",
                st.commits_behind_head
            ))
        } else {
            None
        };
        Ok(ohara_core::query::ResponseMeta {
            index_status: st,
            hint,
        })
    }
}
