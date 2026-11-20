//! `StorageWrite` implementation for [`SqliteStorage`]: repo registration,
//! the main tip, and the per-commit commit + hunk writes.

use crate::storage_impl::with_conn;
use crate::tables::{head_symbol_snapshot, repo};
use crate::SqliteStorage;
use ohara_core::{
    storage::{CommitRecord, HunkRecord, StorageWrite},
    types::{RepoId, Symbol},
    HeadSymbolSnapshot, Result as CoreResult,
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

    /// One transaction on the single write connection: the commit row and
    /// every hunk land together or not at all (`commit::put_tx` +
    /// `hunk::put_many_tx`; the commit goes first for the foreign key).
    async fn put_commit_with_hunks(
        &self,
        _repo_id: &RepoId,
        commit: &CommitRecord,
        hunks: &[HunkRecord],
    ) -> CoreResult<()> {
        let commit = commit.clone();
        let hunks = hunks.to_vec();
        with_conn(self.write_pool(), move |c| {
            let tx = c.transaction()?;
            crate::tables::commit::put_tx(&tx, &commit)?;
            crate::tables::hunk::put_many_tx(&tx, &hunks)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn put_head_symbols(&self, repo_id: &RepoId, symbols: &[Symbol]) -> CoreResult<()> {
        // Replace the whole snapshot (symbols + FTS/vec mirrors) in one
        // transaction; see `tables::symbol::replace_all`. The snapshot no
        // longer matches any recorded provenance, so that goes with it.
        let id = repo_id.as_str().to_string();
        let syms = symbols.to_vec();
        with_conn(self.write_pool(), move |c| {
            let tx = c.transaction()?;
            crate::tables::symbol::replace_all_tx(&tx, &syms)?;
            head_symbol_snapshot::clear(&tx, &id)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// The snapshot and its provenance land in one transaction.
    async fn put_head_symbols_at(
        &self,
        repo_id: &RepoId,
        symbols: &[Symbol],
        provenance: &HeadSymbolSnapshot,
    ) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let key = provenance.worktree.as_str().to_string();
        let sha = provenance.sha.as_str().to_string();
        let syms = symbols.to_vec();
        with_conn(self.write_pool(), move |c| {
            let tx = c.transaction()?;
            crate::tables::symbol::replace_all_tx(&tx, &syms)?;
            head_symbol_snapshot::set(&tx, &id, &key, &sha)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::SqliteStorage;
    use ohara_core::storage::{CommitRecord, HunkRecord, StorageWrite};
    use ohara_core::types::{ChangeKind, CommitMeta, Hunk, RepoId};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MISSING: &str = "ffffffffffffffffffffffffffffffffffffffff";

    async fn setup() -> (tempfile::TempDir, SqliteStorage, RepoId) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        (dir, s, id)
    }

    fn commit(sha: &str) -> CommitRecord {
        CommitRecord {
            meta: CommitMeta {
                commit_sha: sha.into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 1,
                message: format!("commit {sha}"),
            },
            message_emb: vec![0.0; 384],
            ulid: String::new(),
        }
    }

    fn hunk(commit_sha: &str) -> HunkRecord {
        HunkRecord::legacy(
            Hunk {
                commit_sha: commit_sha.into(),
                file_path: "src/a.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn a() {}\n".into(),
            },
            vec![0.5; 384],
        )
    }

    async fn hunk_count(s: &SqliteStorage, sha: &'static str) -> i64 {
        s.pool()
            .get()
            .await
            .unwrap()
            .interact(move |c| {
                c.query_row(
                    "SELECT count(*) FROM hunk WHERE commit_sha = ?1",
                    [sha],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap()
            .unwrap()
    }

    /// A hunk write that fails after the commit row went in must leave no
    /// commit row behind: the indexer's resume skip treats a stored commit
    /// row as done, so a lone commit row would never get its hunks.
    #[tokio::test]
    async fn failed_hunk_write_leaves_no_commit_row_and_a_retry_stores_the_hunks() {
        let (_d, s, id) = setup().await;
        // The second hunk names a commit that has no row: the foreign key
        // fails mid-write, standing in for a crash or SQLITE_BUSY.
        let interrupted = s
            .put_commit_with_hunks(&id, &commit(A), &[hunk(A), hunk(MISSING)])
            .await;
        assert!(interrupted.is_err(), "the foreign key must fail the write");
        assert!(
            !s.commit_exists(A).await.unwrap(),
            "a failed write must not leave a commit row the resume skip would trust"
        );
        assert_eq!(hunk_count(&s, A).await, 0);

        s.put_commit_with_hunks(&id, &commit(A), &[hunk(A), hunk(A)])
            .await
            .unwrap();
        assert!(s.commit_exists(A).await.unwrap());
        assert_eq!(hunk_count(&s, A).await, 2);
    }
}
