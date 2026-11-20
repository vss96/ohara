//! `StorageWrite` implementation for [`SqliteStorage`]: repo registration,
//! the main tip, and the per-commit commit + hunk writes.

use crate::storage_impl::with_conn;
use crate::tables::repo;
use crate::SqliteStorage;
use ohara_core::{
    storage::{CommitRecord, HunkRecord, StorageWrite},
    types::{RepoId, Symbol},
    Result as CoreResult,
};

#[async_trait::async_trait]
impl StorageWrite for SqliteStorage {
    async fn open_repo(
        &self,
        repo_id: &RepoId,
        path: &str,
        first_commit_sha: &str,
    ) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let path = path.to_string();
        let fcs = first_commit_sha.to_string();
        with_conn(self.write_pool(), move |c| {
            repo::upsert(c, &id, &path, &fcs)
        })
        .await
    }

    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let sha = sha.to_string();
        with_conn(self.write_pool(), move |c| {
            repo::set_watermark(c, &id, &sha)
        })
        .await
    }

    async fn put_commit(&self, _repo_id: &RepoId, record: &CommitRecord) -> CoreResult<()> {
        let rec = record.clone();
        with_conn(self.write_pool(), move |c| {
            crate::tables::commit::put(c, &rec)
        })
        .await
    }

    async fn commit_exists(&self, sha: &str) -> CoreResult<bool> {
        let sha = sha.to_string();
        with_conn(self.pool(), move |c| {
            crate::tables::commit::commit_exists(c, &sha)
        })
        .await
    }

    async fn put_hunks(&self, _repo_id: &RepoId, records: &[HunkRecord]) -> CoreResult<()> {
        let recs = records.to_vec();
        with_conn(self.write_pool(), move |c| {
            crate::tables::hunk::put_many(c, &recs)
        })
        .await
    }

    async fn put_head_symbols(&self, _repo_id: &RepoId, symbols: &[Symbol]) -> CoreResult<()> {
        // Replace the whole snapshot (symbols + FTS/vec mirrors) in one
        // transaction; see `tables::symbol::replace_all`.
        let syms = symbols.to_vec();
        with_conn(self.write_pool(), move |c| {
            crate::tables::symbol::replace_all(c, &syms)
        })
        .await
    }
}
