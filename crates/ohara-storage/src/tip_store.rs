//! `TipStore` implementation for [`SqliteStorage`].

use crate::storage_impl::with_conn;
use crate::tables::{indexed_tip, prune, repo};
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
            let Ok(worktree) = WorktreeKey::from_stored(&key) else {
                tracing::warn!(worktree_key = %key, "skipping indexed tip with invalid key");
                continue;
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

    async fn remove_indexed_tip(
        &self,
        repo_id: &RepoId,
        worktree: &WorktreeKey,
        expected: &CommitSha,
    ) -> Result<bool> {
        let id = repo_id.as_str().to_string();
        let key = worktree.as_str().to_string();
        let sha = expected.as_str().to_string();
        with_conn(self.write_pool(), move |c| {
            indexed_tip::remove_if(c, &id, &key, &sha)
        })
        .await
    }

    async fn replace_indexed_tip(
        &self,
        repo_id: &RepoId,
        worktree: &WorktreeKey,
        expected: &CommitSha,
        new: &CommitSha,
    ) -> Result<bool> {
        let id = repo_id.as_str().to_string();
        let key = worktree.as_str().to_string();
        let expected = expected.as_str().to_string();
        let new = new.as_str().to_string();
        with_conn(self.write_pool(), move |c| {
            indexed_tip::replace_if(c, &id, &key, &expected, &new)
        })
        .await
    }

    /// The index holds one repository, so every commit row is this repo's.
    async fn indexed_commit_shas(&self, _repo_id: &RepoId) -> Result<Vec<CommitSha>> {
        let rows = with_conn(self.pool(), |c| prune::list_commit_shas(c)).await?;
        let mut out = Vec::with_capacity(rows.len());
        for sha in rows {
            let Ok(parsed) = CommitSha::parse(&sha) else {
                tracing::warn!(sha = %sha, "skipping commit row with invalid sha");
                continue;
            };
            out.push(parsed);
        }
        Ok(out)
    }

    async fn delete_commits(&self, _repo_id: &RepoId, shas: &[CommitSha]) -> Result<u64> {
        if shas.is_empty() {
            return Ok(0);
        }
        let shas: Vec<String> = shas.iter().map(|s| s.as_str().to_string()).collect();
        with_conn(self.write_pool(), move |c| prune::delete_commits(c, &shas)).await
    }
}

#[cfg(test)]
mod tests {
    use crate::SqliteStorage;
    use ohara_core::storage::{StorageStatus, StorageWrite};
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
        assert!(
            !s.remove_indexed_tip(&id, &slot1(), &sha(C)).await.unwrap(),
            "a tip that moved on is not removed"
        );
        assert!(s.remove_indexed_tip(&id, &slot1(), &sha(B)).await.unwrap());
        assert_eq!(tips(&s, &id).await, vec![("".into(), A.into())]);
    }

    #[tokio::test]
    async fn replace_indexed_tip_moves_only_a_tip_that_still_names_expected() {
        let (_d, s, id) = setup().await;
        let main = WorktreeKey::main();
        s.set_indexed_tip(&id, &main, &sha(B)).await.unwrap();
        assert!(!s
            .replace_indexed_tip(&id, &main, &sha(C), &sha(A))
            .await
            .unwrap());
        assert!(!s
            .replace_indexed_tip(&id, &slot1(), &sha(B), &sha(A))
            .await
            .unwrap());
        assert_eq!(tips(&s, &id).await, vec![("".into(), B.into())]);
        assert!(s
            .replace_indexed_tip(&id, &main, &sha(B), &sha(A))
            .await
            .unwrap());
        assert_eq!(tips(&s, &id).await, vec![("".into(), A.into())]);
        let st = s.get_index_status(&id).await.unwrap();
        assert_eq!(st.last_indexed_commit.as_deref(), Some(A));
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
    async fn retired_tip_round_trips_and_does_not_touch_the_main_tip() {
        let (_d, s, id) = setup().await;
        s.set_indexed_tip(&id, &WorktreeKey::main(), &sha(A))
            .await
            .unwrap();
        let retired = WorktreeKey::retired(&WorktreeKey::main(), &sha(B));
        s.set_indexed_tip(&id, &retired, &sha(B)).await.unwrap();
        let listed = s.indexed_tips(&id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed
            .iter()
            .any(|t| t.worktree == retired && t.worktree.is_retired() && t.sha == sha(B)));
        let st = s.get_index_status(&id).await.unwrap();
        assert_eq!(st.last_indexed_commit.as_deref(), Some(A));
        assert!(s.remove_indexed_tip(&id, &retired, &sha(B)).await.unwrap());
        assert_eq!(s.indexed_tips(&id).await.unwrap().len(), 1);
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

    /// Row count of `table`, read through the reader pool.
    async fn count(s: &SqliteStorage, table: &'static str) -> i64 {
        s.pool()
            .get()
            .await
            .unwrap()
            .interact(move |c| {
                c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            })
            .await
            .unwrap()
            .unwrap()
    }

    /// One commit `sha` with one hunk carrying one attributed symbol.
    async fn put_commit_with_hunk(s: &SqliteStorage, id: &RepoId, sha: &str) {
        use ohara_core::storage::{CommitRecord, HunkRecord};
        use ohara_core::types::{
            AttributionKind, ChangeKind, CommitMeta, Hunk, HunkSymbol, SymbolKind,
        };
        let meta = CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1,
            message: format!("commit {sha}"),
        };
        s.put_commit(
            id,
            &CommitRecord {
                meta,
                message_emb: vec![0.0; 384],
                ulid: String::new(),
            },
        )
        .await
        .unwrap();
        let mut h = HunkRecord::legacy(
            Hunk {
                commit_sha: sha.into(),
                file_path: format!("src/{}.rs", &sha[..1]),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn x() {}\n".into(),
            },
            vec![0.5; 384],
        );
        h.symbols = vec![HunkSymbol {
            kind: SymbolKind::Function,
            name: "x".into(),
            qualified_name: None,
            attribution: AttributionKind::ExactSpan,
        }];
        s.put_hunks(id, &[h]).await.unwrap();
    }

    const PRUNED_TABLES: [&str; 8] = [
        "commit_record",
        "vec_commit",
        "fts_commit",
        "hunk",
        "hunk_symbol",
        "vec_hunk",
        "fts_hunk_text",
        "fts_hunk_semantic",
    ];

    #[tokio::test]
    async fn delete_commits_removes_commit_hunks_vectors_and_fts_rows() {
        let (_d, s, id) = setup().await;
        put_commit_with_hunk(&s, &id, A).await;
        put_commit_with_hunk(&s, &id, B).await;
        let mut before = Vec::new();
        for t in PRUNED_TABLES {
            before.push(count(&s, t).await);
        }

        let deleted = s.delete_commits(&id, &[sha(A)]).await.unwrap();

        assert_eq!(deleted, 1);
        assert!(!s.commit_exists(A).await.unwrap());
        assert!(s.commit_exists(B).await.unwrap());
        for (t, b) in PRUNED_TABLES.iter().zip(before) {
            assert_eq!(count(&s, t).await, b - 1, "{t} must drop by exactly 1");
        }
        let left: Vec<String> = s
            .indexed_commit_shas(&id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.as_str().to_string())
            .collect();
        assert_eq!(left, vec![B.to_string()]);
    }

    #[tokio::test]
    async fn delete_commits_removes_a_whole_batch_and_keeps_the_rest() {
        let (_d, s, id) = setup().await;
        for h in [A, B, C] {
            put_commit_with_hunk(&s, &id, h).await;
        }
        let deleted = s.delete_commits(&id, &[sha(A), sha(C)]).await.unwrap();
        assert_eq!(deleted, 2);
        for t in PRUNED_TABLES {
            assert_eq!(count(&s, t).await, 1, "{t} keeps only B's row");
        }
        assert!(s.commit_exists(B).await.unwrap());
    }

    #[tokio::test]
    async fn delete_commits_of_unknown_shas_deletes_nothing() {
        let (_d, s, id) = setup().await;
        put_commit_with_hunk(&s, &id, A).await;
        assert_eq!(s.delete_commits(&id, &[sha(C)]).await.unwrap(), 0);
        assert_eq!(s.delete_commits(&id, &[]).await.unwrap(), 0);
        assert!(s.commit_exists(A).await.unwrap());
    }
}
