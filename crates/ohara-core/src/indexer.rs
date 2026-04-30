use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;

/// Source of commits + hunks. Implemented by `ohara-git` in a later task; defined
/// here so `ohara-core` stays git-free.
#[async_trait::async_trait]
pub trait CommitSource: Send + Sync {
    /// Yield commits in parents-first order, optionally starting after `since`.
    async fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>>;
    /// Yield the per-file hunks of a single commit.
    async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>>;
}

/// Source of HEAD symbols. Implemented by `ohara-parse` driver in a later task.
#[async_trait::async_trait]
pub trait SymbolSource: Send + Sync {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>>;
}

pub struct Indexer {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn EmbeddingProvider>,
    batch_commits: usize,
    embed_batch: usize,
}

impl Indexer {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self { storage, embedder, batch_commits: 512, embed_batch: 32 }
    }

    /// Run a (full or incremental) indexing pass for `repo_id`.
    /// `commit_source` and `symbol_source` are wired by the caller.
    pub async fn run(
        &self,
        repo_id: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<IndexerReport> {
        let status = self.storage.get_index_status(repo_id).await?;
        let commits = commit_source.list_commits(status.last_indexed_commit.as_deref()).await?;
        tracing::info!(new_commits = commits.len(), "begin index pass");

        let mut latest_sha: Option<String> = status.last_indexed_commit.clone();
        let mut total_hunks = 0usize;

        for chunk in commits.chunks(self.batch_commits) {
            for cm in chunk {
                let hunks = commit_source.hunks_for_commit(&cm.sha).await?;
                total_hunks += hunks.len();

                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(hunks.iter().map(|h| h.diff_text.clone()))
                    .collect();
                let embs = self.embedder.embed_batch(&texts).await?;
                let (msg_emb, hunk_embs) = embs.split_first().expect("non-empty");

                self.storage
                    .put_commit(repo_id, &CommitRecord { meta: cm.clone(), message_emb: msg_emb.clone() })
                    .await?;

                let records: Vec<HunkRecord> = hunks
                    .into_iter()
                    .zip(hunk_embs.iter().cloned())
                    .map(|(h, e)| HunkRecord { hunk: h, diff_emb: e })
                    .collect();
                self.storage.put_hunks(repo_id, &records).await?;
                latest_sha = Some(cm.sha.clone());
            }
        }

        let symbols = symbol_source.extract_head_symbols().await?;
        self.storage.put_head_symbols(repo_id, &symbols).await?;

        if let Some(sha) = latest_sha.as_deref() {
            self.storage.set_last_indexed_commit(repo_id, sha).await?;
        }

        Ok(IndexerReport { new_commits: commits.len(), new_hunks: total_hunks, head_symbols: symbols.len() })
    }
}

#[derive(Debug, Clone)]
pub struct IndexerReport {
    pub new_commits: usize,
    pub new_hunks: usize,
    pub head_symbols: usize,
}
