//! `TipStore` implementation for [`SqliteStorage`].

use crate::storage_impl::with_conn;
use crate::tables::{indexed_tip, repo};
use crate::SqliteStorage;
use async_trait::async_trait;
use ohara_core::{CommitSha, IndexedTip, RepoId, Result, TipStore, WorktreeKey};

#[async_trait]
impl TipStore for SqliteStorage {
    async fn indexed_tips(&self, repo_id: &RepoId) -> Result<Vec<IndexedTip>> {
        let id = repo_id.as_str().to_string();
        let rows = with_conn(self.pool(), move |c| indexed_tip::list(c, &id)).await?;
        let mut tips = Vec::with_capacity(rows.len());
        for (key, sha) in rows {
            let parsed = CommitSha::parse(&sha);
            let Ok(sha) = parsed else {
                tracing::warn!(worktree_key = %key, "skipping indexed tip with invalid sha");
                continue;
            };
            let worktree = match key.is_empty() {
                true => WorktreeKey::main(),
                false => match WorktreeKey::linked(&key) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
            };
            tips.push(IndexedTip { worktree, sha });
        }
        Ok(tips)
    }

    async fn set_indexed_tip(
        &self,
        repo_id: &RepoId,
        worktree: &WorktreeKey,
        sha: &CommitSha,
    ) -> Result<()> {
        let id = repo_id.as_str().to_string();
        let key = worktree.as_str().to_string();
        let sha = sha.as_str().to_string();
        with_conn(self.write_pool(), move |c| match key.is_empty() {
            true => repo::set_watermark(c, &id, &sha),
            false => indexed_tip::upsert(c, &id, &key, &sha),
        })
        .await
    }

    async fn remove_indexed_tip(&self, repo_id: &RepoId, worktree: &WorktreeKey) -> Result<()> {
        let id = repo_id.as_str().to_string();
        let key = worktree.as_str().to_string();
        with_conn(self.write_pool(), move |c| {
            indexed_tip::remove(c, &id, &key)
        })
        .await
    }

    async fn indexed_commit_shas(&self, _repo_id: &RepoId) -> Result<Vec<CommitSha>> {
        Ok(vec![]) // Task 11
    }

    async fn delete_commits(&self, _repo_id: &RepoId, _shas: &[CommitSha]) -> Result<u64> {
        Ok(0) // Task 11
    }
}

#[cfg(test)]
mod tests {
    use crate::SqliteStorage;
    use ohara_core::storage::Storage;
    use ohara_core::{CommitSha, RepoId, TipStore, WorktreeKey};
    use std::sync::Arc;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    async fn setup() -> (tempfile::TempDir, SqliteStorage, RepoId) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        (dir, s, id)
    }

    fn sha(h: &str) -> CommitSha {
        CommitSha::parse(h).unwrap()
    }

    fn slot1() -> WorktreeKey {
        WorktreeKey::linked("slot-1").unwrap()
    }

    async fn tips(s: &SqliteStorage, id: &RepoId) -> Vec<(String, String)> {
        s.indexed_tips(id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.worktree.as_str().to_string(), t.sha.as_str().to_string()))
            .collect()
    }

    #[tokio::test]
    async fn set_and_list_tips_per_worktree() {
        let (_d, s, id) = setup().await;
        s.set_indexed_tip(&id, &WorktreeKey::main(), &sha(A))
            .await
            .unwrap();
        s.set_indexed_tip(&id, &slot1(), &sha(B)).await.unwrap();
        s.set_indexed_tip(&id, &slot1(), &sha(C)).await.unwrap();
        assert_eq!(
            tips(&s, &id).await,
            vec![("".into(), A.into()), ("slot-1".into(), C.into())]
        );
    }

    #[tokio::test]
    async fn remove_indexed_tip_deletes_only_that_worktree() {
        let (_d, s, id) = setup().await;
        s.set_indexed_tip(&id, &WorktreeKey::main(), &sha(A))
            .await
            .unwrap();
        s.set_indexed_tip(&id, &slot1(), &sha(B)).await.unwrap();
        s.remove_indexed_tip(&id, &slot1()).await.unwrap();
        assert_eq!(tips(&s, &id).await, vec![("".into(), A.into())]);
    }

    #[tokio::test]
    async fn legacy_watermark_methods_read_and_write_the_main_tip() {
        let (_d, s, id) = setup().await;
        s.set_last_indexed_commit(&id, A).await.unwrap();
        assert_eq!(tips(&s, &id).await, vec![("".into(), A.into())]);

        s.set_indexed_tip(&id, &WorktreeKey::main(), &sha(B))
            .await
            .unwrap();
        let st = s.get_index_status(&id).await.unwrap();
        assert_eq!(st.last_indexed_commit.as_deref(), Some(B));
        assert!(st.indexed_at.is_some());

        s.set_indexed_tip(&id, &slot1(), &sha(C)).await.unwrap();
        let st = s.get_index_status(&id).await.unwrap();
        assert_eq!(st.last_indexed_commit.as_deref(), Some(B));
    }

    #[tokio::test]
    async fn tips_route_through_the_single_writer_pool() {
        let (_d, s, id) = setup().await;
        let s = Arc::new(s);
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = s.clone();
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                let key = WorktreeKey::linked(&format!("slot-{i}")).unwrap();
                s.set_indexed_tip(&id, &key, &CommitSha::parse(A).unwrap())
                    .await
            }));
        }
        for h in handles {
            h.await
                .unwrap()
                .expect("every concurrent tip write succeeds");
        }
        assert_eq!(tips(&s, &id).await.len(), 16);
    }
}
