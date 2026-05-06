use anyhow::Result;
use ohara_core::storage::CommitRecord;
use ohara_core::types::CommitMeta;
use rusqlite::{params, Connection, OptionalExtension};

use crate::codec::vec_codec;

pub fn put(c: &mut Connection, record: &CommitRecord) -> Result<()> {
    let tx = c.transaction()?;
    // Resume safety: clear sqlite-vec + FTS5 mirrors first, then INSERT.
    // sqlite-vec's vec0 virtual tables do NOT honor `INSERT OR REPLACE`
    // — re-inserting the same `commit_sha` raises "UNIQUE constraint
    // failed on vec_commit primary key" even with OR REPLACE. So we
    // explicitly DELETE-then-INSERT, matching the pattern hunk::put_many
    // already uses for vec_hunk + fts_hunk_text. commit_record is a
    // regular table so OR REPLACE there is fine.
    tx.execute(
        "DELETE FROM vec_commit WHERE commit_sha = ?1",
        params![&record.meta.commit_sha],
    )?;
    tx.execute(
        "DELETE FROM fts_commit WHERE sha = ?1",
        params![&record.meta.commit_sha],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO commit_record (sha, parent_sha, is_merge, ts, author, message, ulid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            &record.meta.commit_sha,
            &record.meta.parent_sha,
            record.meta.is_merge as i64,
            record.meta.ts,
            &record.meta.author,
            &record.meta.message,
            record.ulid.as_str(),
        ],
    )?;
    let bytes = vec_codec::vec_to_bytes(&record.message_emb);
    tx.execute(
        "INSERT INTO vec_commit (commit_sha, message_emb) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, bytes],
    )?;
    tx.execute(
        "INSERT INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.commit_sha, &record.meta.message],
    )?;
    tx.commit()?;
    Ok(())
}

/// Cheap "is this commit already indexed?" PK lookup used by the
/// indexer's resume short-circuit (plan-9 / RFC v0.6.3). Returns true
/// when `commit_record.sha = ?1` exists. The lookup hits the primary-key
/// index, so cost is sub-millisecond per call — acceptable on cold
/// indexes too (a 5,800-commit walk pays ~0.1 s in PK lookups).
pub fn commit_exists(c: &Connection, sha: &str) -> Result<bool> {
    let found: Option<i64> = c
        .query_row(
            "SELECT 1 FROM commit_record WHERE sha = ?1 LIMIT 1",
            params![sha],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Plan 28: return the commit with the highest ULID (excluding empty-ULID
/// pre-V6 rows). Returns `Ok(None)` when the index is empty or all rows
/// pre-date V6 and have not been rebuilt.
///
/// `commit_record` has no `repo_id` column — the table is global and the
/// ULID ordering is a time-stable proxy for "most recently committed".
pub fn latest_by_ulid(c: &Connection) -> Result<Option<CommitMeta>> {
    // ulid != '' excludes pre-V6 rows written without a ULID.
    let row = c
        .query_row(
            "SELECT sha, parent_sha, is_merge, ts, author, message
             FROM commit_record
             WHERE ulid != ''
             ORDER BY ulid DESC LIMIT 1",
            [],
            |r| {
                let commit_sha: String = r.get(0)?;
                let parent_sha: Option<String> = r.get(1)?;
                let is_merge: i64 = r.get(2)?;
                let ts: i64 = r.get(3)?;
                let author: Option<String> = r.get(4)?;
                let message: String = r.get(5)?;
                Ok(CommitMeta {
                    commit_sha,
                    parent_sha,
                    is_merge: is_merge != 0,
                    author,
                    ts,
                    message,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Fetch a single commit's metadata by SHA. Returns `Ok(None)` if the
/// SHA isn't present in `commit_record` (e.g., the commit is older than
/// the local index watermark). Used by the `explain_change` orchestrator
/// to enrich `git blame` results with author / message / timestamp for
/// display.
pub fn get(c: &Connection, sha: &str) -> Result<Option<CommitMeta>> {
    let row = c
        .query_row(
            "SELECT sha, parent_sha, is_merge, ts, author, message
             FROM commit_record
             WHERE sha = ?1",
            params![sha],
            |r| {
                let commit_sha: String = r.get(0)?;
                let parent_sha: Option<String> = r.get(1)?;
                let is_merge: i64 = r.get(2)?;
                let ts: i64 = r.get(3)?;
                let author: Option<String> = r.get(4)?;
                let message: String = r.get(5)?;
                Ok(CommitMeta {
                    commit_sha,
                    parent_sha,
                    is_merge: is_merge != 0,
                    author,
                    ts,
                    message,
                })
            },
        )
        .optional()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use crate::SqliteStorage;
    use ohara_core::storage::{CommitRecord, Storage};
    use ohara_core::types::{CommitMeta, RepoId};
    use ohara_core::ulid_for_commit;

    #[tokio::test]
    async fn latest_by_ulid_returns_highest_ulid() {
        // Plan 28 Task B.1: insert two commits at different timestamps;
        // latest_by_ulid must return the later one (higher ULID).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ulid_test.db");
        let s = SqliteStorage::open(&path).await.unwrap();
        let repo_id = RepoId::from_parts("0".repeat(40).as_str(), "/tmp/ulid");
        s.open_repo(&repo_id, "/tmp/ulid", &"0".repeat(40))
            .await
            .unwrap();

        let earlier_ts: i64 = 1_700_000_000;
        let later_ts: i64 = 1_710_000_000;

        let earlier_sha = "aaaa1111".repeat(5);
        let later_sha = "bbbb2222".repeat(5);

        for (sha, ts) in [(&earlier_sha, earlier_ts), (&later_sha, later_ts)] {
            let meta = CommitMeta {
                commit_sha: sha.clone(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts,
                message: format!("commit at {ts}"),
            };
            let ulid = ulid_for_commit(ts, sha).to_string();
            s.put_commit(
                &repo_id,
                &CommitRecord {
                    ulid,
                    meta,
                    message_emb: vec![0.0_f32; 384],
                },
            )
            .await
            .unwrap();
        }

        let conn = s.pool().get().await.unwrap();
        let result = conn
            .interact(|c| super::latest_by_ulid(c))
            .await
            .unwrap()
            .unwrap();

        assert!(
            result.is_some(),
            "latest_by_ulid must return Some when commits are indexed"
        );
        assert_eq!(
            result.unwrap().commit_sha,
            later_sha,
            "latest_by_ulid must return the commit with the highest ULID"
        );
    }

    async fn temp_storage() -> (tempfile::TempDir, SqliteStorage) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("commit_test.db");
        let s = SqliteStorage::open(&path).await.unwrap();
        (dir, s)
    }

    #[tokio::test]
    async fn put_commit_writes_ulid_column() {
        let (_dir, storage) = temp_storage().await;
        let repo_id = RepoId::from_parts("0".repeat(40).as_str(), "/tmp/x");
        storage
            .open_repo(&repo_id, "/tmp/x", &"0".repeat(40))
            .await
            .unwrap();

        let meta = CommitMeta {
            commit_sha: "deadbeef".repeat(5),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "hello".into(),
        };
        let ulid = ulid_for_commit(meta.ts, &meta.commit_sha).to_string();
        let record = CommitRecord {
            ulid: ulid.clone(),
            meta,
            message_emb: vec![0.0_f32; 384],
        };
        storage.put_commit(&repo_id, &record).await.unwrap();

        let conn = storage.pool().get().await.unwrap();
        let sha = "deadbeef".repeat(5);
        let stored: String = conn
            .interact(move |c| {
                c.query_row(
                    "SELECT ulid FROM commit_record WHERE sha = ?1",
                    [&sha],
                    |r| r.get::<_, String>(0),
                )
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, ulid);
    }
}
