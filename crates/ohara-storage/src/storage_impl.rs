use crate::pool::SqlitePoolBuilder;
use crate::{migrations, repo};
use anyhow::Result;
use deadpool_sqlite::Pool;
use ohara_core::{
    query::IndexStatus,
    storage::{CommitRecord, HunkHit, HunkRecord, Storage},
    types::{RepoId, Symbol},
    Result as CoreResult,
};
use std::path::Path;

pub struct SqliteStorage {
    pool: Pool,
}

impl SqliteStorage {
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let pool = SqlitePoolBuilder::new(path).build().await?;
        let conn = pool.get().await?;
        conn.interact(migrations::run)
            .await
            .map_err(|e| anyhow::anyhow!("interact: {e}"))??;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &Pool { &self.pool }
}

async fn with_conn<F, T>(pool: &deadpool_sqlite::Pool, f: F) -> ohara_core::Result<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    pool.get()
        .await
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
        .interact(f)
        .await
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
}

#[async_trait::async_trait]
impl Storage for SqliteStorage {
    async fn open_repo(&self, repo_id: &RepoId, path: &str, first_commit_sha: &str) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let path = path.to_string();
        let fcs = first_commit_sha.to_string();
        with_conn(&self.pool, move |c| repo::upsert(c, &id, &path, &fcs)).await
    }

    async fn get_index_status(&self, repo_id: &RepoId) -> CoreResult<IndexStatus> {
        let id = repo_id.as_str().to_string();
        with_conn(&self.pool, move |c| repo::get_status(c, &id)).await
    }

    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let sha = sha.to_string();
        with_conn(&self.pool, move |c| repo::set_watermark(c, &id, &sha)).await
    }

    async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> CoreResult<()> {
        // populated in Task 8
        unimplemented!()
    }
    async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> CoreResult<()> { unimplemented!() }
    async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> CoreResult<()> { unimplemented!() }
    async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> CoreResult<Vec<HunkHit>> { unimplemented!() }
    async fn blob_was_seen(&self, _: &str, _: &str) -> CoreResult<bool> { unimplemented!() }
    async fn record_blob_seen(&self, _: &str, _: &str) -> CoreResult<()> { unimplemented!() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_repo_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite")).await.unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        let st = s.get_index_status(&id).await.unwrap();
        assert!(st.last_indexed_commit.is_none());
        s.set_last_indexed_commit(&id, "abc").await.unwrap();
        let st2 = s.get_index_status(&id).await.unwrap();
        assert_eq!(st2.last_indexed_commit.as_deref(), Some("abc"));
    }
}
