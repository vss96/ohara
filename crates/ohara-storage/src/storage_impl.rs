use crate::codec::pool::SqlitePoolBuilder;
use crate::migrations;
use crate::tables::repo;
use anyhow::Result;
use deadpool_sqlite::Pool;
use ohara_core::{
    index_metadata::StoredIndexMetadata,
    query::IndexStatus,
    storage::{CommitRecord, HunkHit, HunkId, HunkRecord, Storage},
    types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol},
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

    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

async fn with_conn<F, T>(pool: &deadpool_sqlite::Pool, f: F) -> ohara_core::Result<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    pool.get()
        .await
        .map_err(|e| ohara_core::OhraError::Storage(format!("pool: {e}")))?
        .interact(f)
        .await
        .map_err(|e| ohara_core::OhraError::Storage(format!("interact: {e}")))?
        .map_err(|e| ohara_core::OhraError::Storage(format!("query: {e}")))
}

#[async_trait::async_trait]
impl Storage for SqliteStorage {
    async fn open_repo(
        &self,
        repo_id: &RepoId,
        path: &str,
        first_commit_sha: &str,
    ) -> CoreResult<()> {
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

    async fn put_commit(&self, _repo_id: &RepoId, record: &CommitRecord) -> CoreResult<()> {
        let rec = record.clone();
        with_conn(&self.pool, move |c| crate::tables::commit::put(c, &rec)).await
    }

    async fn commit_exists(&self, sha: &str) -> CoreResult<bool> {
        let sha = sha.to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::commit::commit_exists(c, &sha)
        })
        .await
    }
    async fn put_hunks(&self, _repo_id: &RepoId, records: &[HunkRecord]) -> CoreResult<()> {
        let recs = records.to_vec();
        with_conn(&self.pool, move |c| crate::tables::hunk::put_many(c, &recs)).await
    }

    async fn put_head_symbols(&self, _repo_id: &RepoId, symbols: &[Symbol]) -> CoreResult<()> {
        // Plan 3 / Track A: persist symbols + mirror into fts_symbol_name so
        // the BM25-by-symbol-name lane has rows to match against. Track C
        // wires `Symbol::sibling_names` through this path.
        let syms = symbols.to_vec();
        with_conn(&self.pool, move |c| {
            crate::tables::symbol::put_many(c, &syms)
        })
        .await
    }

    async fn clear_head_symbols(&self, _repo_id: &RepoId) -> CoreResult<()> {
        // Plan 3 / Track D: drop all symbol rows + their FTS5 mirror so a
        // subsequent put_head_symbols (driven by `ohara index --force`)
        // doesn't double-count. The symbol table is HEAD-scoped — it holds
        // only the latest snapshot, never historical, so a blanket DELETE
        // is the right semantics.
        with_conn(&self.pool, crate::tables::symbol::clear_all).await
    }

    async fn knn_hunks(
        &self,
        _repo_id: &RepoId,
        query_emb: &[f32],
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let qe = query_emb.to_vec();
        let lang = language.map(str::to_string);
        with_conn(&self.pool, move |c| {
            crate::tables::hunk::knn(c, &qe, k, lang.as_deref(), since_unix)
        })
        .await
    }

    async fn bm25_hunks_by_text(
        &self,
        _repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let q = query.to_string();
        let lang = language.map(str::to_string);
        with_conn(&self.pool, move |c| {
            crate::tables::hunk::bm25_by_text(c, &q, k, lang.as_deref(), since_unix)
        })
        .await
    }

    async fn bm25_hunks_by_semantic_text(
        &self,
        _repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let q = query.to_string();
        let lang = language.map(str::to_string);
        with_conn(&self.pool, move |c| {
            crate::tables::hunk::bm25_by_semantic_text(c, &q, k, lang.as_deref(), since_unix)
        })
        .await
    }

    async fn bm25_hunks_by_symbol_name(
        &self,
        _repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let q = query.to_string();
        let lang = language.map(str::to_string);
        with_conn(&self.pool, move |c| {
            crate::tables::symbol::bm25_by_name(c, &q, k, lang.as_deref(), since_unix)
        })
        .await
    }

    async fn bm25_hunks_by_historical_symbol(
        &self,
        _repo_id: &RepoId,
        query: &str,
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let q = query.to_string();
        let lang = language.map(str::to_string);
        with_conn(&self.pool, move |c| {
            crate::tables::hunk_symbol::bm25_by_historical_symbol(
                c,
                &q,
                k,
                lang.as_deref(),
                since_unix,
            )
        })
        .await
    }

    async fn get_hunk_symbols(
        &self,
        _repo_id: &RepoId,
        hunk_id: HunkId,
    ) -> CoreResult<Vec<HunkSymbol>> {
        with_conn(&self.pool, move |c| {
            crate::tables::hunk_symbol::get_for_hunk(c, hunk_id)
        })
        .await
    }

    async fn blob_was_seen(&self, blob_sha: &str, model: &str) -> CoreResult<bool> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::blob_cache::was_seen(c, &blob, &m)
        })
        .await
    }

    async fn record_blob_seen(&self, blob_sha: &str, model: &str) -> CoreResult<()> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::blob_cache::record(c, &blob, &m)
        })
        .await
    }

    async fn get_commit(&self, _repo_id: &RepoId, sha: &str) -> CoreResult<Option<CommitMeta>> {
        // Plan 5 / Task 2: SELECT a single commit row by sha. Returns
        // Ok(None) for SHAs that aren't yet indexed so the explain_change
        // orchestrator can skip them gracefully.
        let sha = sha.to_string();
        with_conn(&self.pool, move |c| crate::tables::commit::get(c, &sha)).await
    }

    async fn get_hunks_for_file_in_commit(
        &self,
        _repo_id: &RepoId,
        sha: &str,
        file_path: &str,
    ) -> CoreResult<Vec<Hunk>> {
        // Plan 5 / Task 3: scoped lookup of (commit_sha, file_path) hunks
        // — the join key the explain_change orchestrator uses to attach a
        // diff excerpt per blame hit.
        let sha = sha.to_string();
        let path = file_path.to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::explain::get_hunks_for_file_in_commit(c, &sha, &path)
        })
        .await
    }

    async fn get_neighboring_file_commits(
        &self,
        _repo_id: &RepoId,
        file_path: &str,
        anchor_sha: &str,
        limit_before: u8,
        limit_after: u8,
    ) -> CoreResult<Vec<(u32, CommitMeta)>> {
        let path = file_path.to_string();
        let sha = anchor_sha.to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::explain::get_neighboring_file_commits(
                c,
                &path,
                &sha,
                limit_before,
                limit_after,
            )
        })
        .await
    }

    async fn get_index_metadata(&self, repo_id: &RepoId) -> CoreResult<StoredIndexMetadata> {
        let id = repo_id.as_str().to_string();
        with_conn(&self.pool, move |c| {
            crate::tables::index_metadata::get(c, &id)
        })
        .await
    }

    async fn put_index_metadata(
        &self,
        repo_id: &RepoId,
        components: &[(String, String)],
    ) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let comps = components.to_vec();
        with_conn(&self.pool, move |c| {
            crate::tables::index_metadata::put_many(c, &id, &comps)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_repo_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        let st = s.get_index_status(&id).await.unwrap();
        assert!(st.last_indexed_commit.is_none());
        s.set_last_indexed_commit(&id, "abc").await.unwrap();
        let st2 = s.get_index_status(&id).await.unwrap();
        assert_eq!(st2.last_indexed_commit.as_deref(), Some("abc"));
    }

    use ohara_core::types::CommitMeta;

    #[tokio::test]
    async fn put_commit_persists_meta_and_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();

        let cm = CommitMeta {
            commit_sha: "abc".into(),
            parent_sha: None,
            is_merge: false,
            author: Some("alice".into()),
            ts: 1_700_000_000,
            message: "first commit".into(),
        };
        let emb = vec![0.1f32; 384];
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm.clone(),
                message_emb: emb,
            },
        )
        .await
        .unwrap();

        let pool = s.pool().clone();
        let count: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM commit_record", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
        let vec_count: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM vec_commit", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(vec_count, 1);
    }

    #[tokio::test]
    async fn put_commit_is_idempotent_under_resume() {
        // Resume safety: re-running put_commit on the same SHA after a
        // mid-walk abort must not raise "UNIQUE constraint failed on
        // vec_commit primary key" (sqlite-vec's vec0 virtual tables don't
        // honor INSERT OR REPLACE — we DELETE-then-INSERT instead).
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();

        let cm = CommitMeta {
            commit_sha: "dup".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1,
            message: "first".into(),
        };
        // First put — populates commit_record + vec_commit + fts_commit.
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm.clone(),
                message_emb: vec![0.1; 384],
            },
        )
        .await
        .unwrap();

        // Second put with the SAME sha — must succeed (the resume case).
        // Different message + embedding to verify replace-semantics.
        let cm2 = CommitMeta {
            message: "second".into(),
            ..cm
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm2,
                message_emb: vec![0.9; 384],
            },
        )
        .await
        .unwrap();

        let pool = s.pool().clone();
        // Exactly one row in each table — replace, not duplicate.
        let n_commit: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM commit_record", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n_commit, 1, "commit_record must dedupe by sha");
        let n_vec: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM vec_commit", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n_vec, 1, "vec_commit must dedupe by sha (sqlite-vec quirk)");
        let n_fts: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM fts_commit", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n_fts, 1, "fts_commit must dedupe by sha");

        // Replace semantics: the second put's message wins.
        let msg: String = pool
            .get()
            .await
            .unwrap()
            .interact(|c| {
                c.query_row(
                    "SELECT message FROM commit_record WHERE sha = 'dup'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg, "second");
    }

    #[tokio::test]
    async fn put_commit_embedding_round_trips_through_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();

        let original: Vec<f32> = (0..384).map(|i| (i as f32) * 0.001 - 0.2).collect();
        let cm = CommitMeta {
            commit_sha: "rt".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "rt".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: original.clone(),
            },
        )
        .await
        .unwrap();

        let pool = s.pool().clone();
        let recovered: Vec<f32> = pool
            .get()
            .await
            .unwrap()
            .interact(|c| {
                let bytes: Vec<u8> = c.query_row(
                    "SELECT message_emb FROM vec_commit WHERE commit_sha = 'rt'",
                    [],
                    |r| r.get(0),
                )?;
                Ok::<_, rusqlite::Error>(crate::codec::vec_codec::bytes_to_vec(&bytes))
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(recovered.len(), 384);
        for (i, (a, b)) in original.iter().zip(recovered.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-6,
                "mismatch at index {i}: orig={a}, got={b}"
            );
        }
    }

    use ohara_core::types::{ChangeKind, Hunk, Symbol, SymbolKind};

    async fn fixture_storage_with_repo() -> (tempfile::TempDir, SqliteStorage, RepoId) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        (dir, s, id)
    }

    #[tokio::test]
    async fn put_hunks_creates_file_paths_and_vec_rows() {
        let (_dir, s, id) = fixture_storage_with_repo().await;

        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1,
            message: "m".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();

        let h = HunkRecord::legacy(
            Hunk {
                commit_sha: "c1".into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn x() {}\n".into(),
            },
            vec![0.5f32; 384],
        );
        s.put_hunks(&id, &[h]).await.unwrap();

        let pool = s.pool().clone();
        let n: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM hunk", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 1);
        let vn: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM vec_hunk", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(vn, 1);
    }

    #[tokio::test]
    async fn knn_hunks_returns_nearest() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1,
            message: "m".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();

        let mk_hunk = |emb_val: f32, name: &str| {
            HunkRecord::legacy(
                Hunk {
                    commit_sha: "c1".into(),
                    file_path: format!("src/{name}.rs"),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Added,
                    diff_text: format!("+fn {name}() {{}}\n"),
                },
                vec![emb_val; 384],
            )
        };
        s.put_hunks(
            &id,
            &[mk_hunk(0.1, "a"), mk_hunk(0.5, "b"), mk_hunk(0.9, "c")],
        )
        .await
        .unwrap();

        let q = vec![0.49f32; 384];
        let hits = s.knn_hunks(&id, &q, 2, None, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].hunk.file_path.ends_with("b.rs"));
    }

    #[tokio::test]
    async fn blob_cache_round_trips() {
        let (_dir, s, _id) = fixture_storage_with_repo().await;
        assert!(!s.blob_was_seen("blob1", "bge-small-v1.5").await.unwrap());
        s.record_blob_seen("blob1", "bge-small-v1.5").await.unwrap();
        assert!(s.blob_was_seen("blob1", "bge-small-v1.5").await.unwrap());
        assert!(!s.blob_was_seen("blob1", "voyage-code-3").await.unwrap());
    }

    #[tokio::test]
    async fn knn_hunks_similarity_is_bounded_in_zero_to_one() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1,
            message: "m".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();

        // Three hunks with very different magnitudes — produces a wide range of L2 distances.
        let mk = |val: f32, name: &str| {
            HunkRecord::legacy(
                Hunk {
                    commit_sha: "c1".into(),
                    file_path: format!("{name}.rs"),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Added,
                    diff_text: format!("+{name}"),
                },
                vec![val; 384],
            )
        };
        s.put_hunks(
            &id,
            &[mk(0.0, "near"), mk(1.0, "far"), mk(10.0, "very_far")],
        )
        .await
        .unwrap();

        let q = vec![0.0_f32; 384];
        let hits = s.knn_hunks(&id, &q, 3, None, None).await.unwrap();
        assert_eq!(hits.len(), 3);

        // Every similarity must be in (0.0, 1.0].
        for h in &hits {
            assert!(
                h.similarity > 0.0 && h.similarity <= 1.0,
                "similarity {} out of bounds for {:?}",
                h.similarity,
                h.hunk.file_path,
            );
        }

        // Closest match (distance 0) should have similarity == 1.0.
        let near = hits.iter().find(|h| h.hunk.file_path == "near.rs").unwrap();
        assert!(
            (near.similarity - 1.0).abs() < 1e-6,
            "near.rs should have similarity ≈ 1.0, got {}",
            near.similarity
        );

        // Ordering: nearest first.
        assert_eq!(hits[0].hunk.file_path, "near.rs");
    }

    /// Helper: seed a single commit + a set of hunks with distinct diff
    /// texts so BM25 lane tests can pick a winner. Returns the commit
    /// timestamp used so callers can derive `since_unix` boundaries.
    async fn seed_hunks_with_texts(
        s: &SqliteStorage,
        id: &RepoId,
        commit_sha: &str,
        ts: i64,
        hunks: &[(&str, &str, Option<&str>)], // (file_name, diff_text, language)
    ) {
        let cm = CommitMeta {
            commit_sha: commit_sha.into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts,
            message: "m".into(),
        };
        s.put_commit(
            id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();

        let recs: Vec<HunkRecord> = hunks
            .iter()
            .map(|(name, diff, lang)| {
                HunkRecord::legacy(
                    Hunk {
                        commit_sha: commit_sha.into(),
                        file_path: format!("src/{name}.rs"),
                        language: lang.map(str::to_string),
                        change_kind: ChangeKind::Added,
                        diff_text: (*diff).to_string(),
                    },
                    vec![0.0f32; 384],
                )
            })
            .collect();
        s.put_hunks(id, &recs).await.unwrap();
    }

    #[tokio::test]
    async fn bm25_hunks_by_text_orders_best_first() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        seed_hunks_with_texts(
            &s,
            &id,
            "c1",
            1_700_000_000,
            &[
                (
                    "a",
                    "+fn retry_with_backoff() { /* retry */ }\n",
                    Some("rust"),
                ),
                ("b", "+fn renamed_helper() {}\n", Some("rust")),
                ("c", "+fn timeout_helper() {}\n", Some("rust")),
            ],
        )
        .await;

        let hits = s
            .bm25_hunks_by_text(&id, "retry", 5, None, None)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "BM25 should match the retry hunk");
        assert!(
            hits[0].hunk.file_path.ends_with("a.rs"),
            "rank 0 must be the retry hunk, got {:?}",
            hits[0].hunk.file_path
        );
        // Score must be positive ("higher is better" convention).
        assert!(hits[0].similarity > 0.0);
        // hunk_id must be populated for the RRF join key contract.
        assert!(hits[0].hunk_id > 0, "hunk_id must be a real rowid");
    }

    #[tokio::test]
    async fn bm25_hunks_by_symbol_name_filters_by_language() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        seed_hunks_with_texts(
            &s,
            &id,
            "c1",
            1_700_000_000,
            &[
                ("a", "+fn alpha_handler() {}\n", Some("rust")),
                ("b", "+def beta_handler():\n", Some("python")),
            ],
        )
        .await;
        // Persist symbols whose names match the queries — one per language.
        s.put_head_symbols(
            &id,
            &[
                Symbol {
                    file_path: "src/a.rs".into(),
                    language: "rust".into(),
                    kind: SymbolKind::Function,
                    name: "alpha_handler".into(),
                    qualified_name: None,
                    sibling_names: Vec::new(),
                    span_start: 0,
                    span_end: 20,
                    blob_sha: "sha-a".into(),
                    source_text: "fn alpha_handler() {}".into(),
                },
                Symbol {
                    file_path: "src/b.rs".into(),
                    language: "python".into(),
                    kind: SymbolKind::Function,
                    name: "beta_handler".into(),
                    qualified_name: None,
                    sibling_names: Vec::new(),
                    span_start: 0,
                    span_end: 20,
                    blob_sha: "sha-b".into(),
                    source_text: "def beta_handler():".into(),
                },
            ],
        )
        .await
        .unwrap();

        let rust_hits = s
            .bm25_hunks_by_symbol_name(&id, "alpha_handler", 5, Some("rust"), None)
            .await
            .unwrap();
        assert_eq!(rust_hits.len(), 1, "rust filter should keep only a.rs");
        assert!(rust_hits[0].hunk.file_path.ends_with("a.rs"));

        let py_hits = s
            .bm25_hunks_by_symbol_name(&id, "beta_handler", 5, Some("python"), None)
            .await
            .unwrap();
        assert_eq!(py_hits.len(), 1);
        assert!(py_hits[0].hunk.file_path.ends_with("b.rs"));
    }

    #[tokio::test]
    async fn bm25_hunks_by_text_respects_since_unix() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        // Two commits: one old, one recent. Both touch a hunk whose diff
        // text contains "retry". The since_unix filter must drop the old.
        seed_hunks_with_texts(
            &s,
            &id,
            "old",
            1_000_000_000, // ts: ~2001
            &[("a", "+fn retry_old() {}\n", Some("rust"))],
        )
        .await;
        seed_hunks_with_texts(
            &s,
            &id,
            "new",
            1_700_000_000, // ts: ~2023
            &[("b", "+fn retry_new() {}\n", Some("rust"))],
        )
        .await;

        let cutoff = 1_500_000_000;
        let hits = s
            .bm25_hunks_by_text(&id, "retry", 10, None, Some(cutoff))
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "since_unix filter must drop pre-cutoff commits"
        );
        assert_eq!(hits[0].commit.commit_sha, "new");
    }

    #[tokio::test]
    async fn get_commit_returns_none_for_unindexed_sha() {
        // Plan 5 / Task 2.r: explain_change skips unindexed commits, so the
        // storage layer must surface "not found" as Ok(None) (not an error).
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let out = s.get_commit(&id, "deadbeefnonexistent").await.unwrap();
        assert!(out.is_none(), "unindexed sha must return Ok(None)");
    }

    #[tokio::test]
    async fn get_hunks_for_file_in_commit_filters_by_path() {
        // Plan 5 / Task 3.r: a single commit can touch many files; explain
        // wants hunks scoped to the queried file path only. Seed a commit
        // with two hunks (different files); the lookup must return just
        // the matching file's hunk.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "filter-sha".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "two files in one commit".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
        let hunks = vec![
            HunkRecord::legacy(
                Hunk {
                    commit_sha: "filter-sha".into(),
                    file_path: "src/auth.rs".into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Modified,
                    diff_text: "+    retry();\n".into(),
                },
                vec![0.0; 384],
            ),
            HunkRecord::legacy(
                Hunk {
                    commit_sha: "filter-sha".into(),
                    file_path: "src/other.rs".into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Added,
                    diff_text: "+    other();\n".into(),
                },
                vec![0.0; 384],
            ),
        ];
        s.put_hunks(&id, &hunks).await.unwrap();

        let got = s
            .get_hunks_for_file_in_commit(&id, "filter-sha", "src/auth.rs")
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "only the auth.rs hunk should match");
        assert_eq!(got[0].file_path, "src/auth.rs");
        assert_eq!(got[0].commit_sha, "filter-sha");
        assert!(got[0].diff_text.contains("retry"));
    }

    #[tokio::test]
    async fn get_hunks_for_file_in_commit_returns_empty_for_unknown_sha() {
        // Plan 5 / Task 3.r: an unknown sha must yield an empty Vec, not
        // an error. The explain_change orchestrator already skips unindexed
        // commits via get_commit, but defense-in-depth says the hunks
        // lookup should fail open the same way.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let got = s
            .get_hunks_for_file_in_commit(&id, "no-such-sha", "src/foo.rs")
            .await
            .unwrap();
        assert!(got.is_empty(), "unknown sha must return empty Vec");
    }

    #[tokio::test]
    async fn get_commit_round_trips() {
        // Plan 5 / Task 2.g: persist a commit then fetch it back; every
        // CommitMeta field must round-trip identically.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "rt-sha".into(),
            parent_sha: Some("parent-sha".into()),
            is_merge: true,
            author: Some("alice@example.com".into()),
            ts: 1_700_000_042,
            message: "Switch fetch to retry with backoff".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm.clone(),
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
        let got = s.get_commit(&id, "rt-sha").await.unwrap().expect("present");
        assert_eq!(got.commit_sha, cm.commit_sha);
        assert_eq!(got.parent_sha, cm.parent_sha);
        assert_eq!(got.is_merge, cm.is_merge);
        assert_eq!(got.author, cm.author);
        assert_eq!(got.ts, cm.ts);
        assert_eq!(got.message, cm.message);
    }

    #[tokio::test]
    async fn with_conn_tags_query_errors_with_stage_prefix() {
        // A failing rusqlite call inside the closure should bubble out as
        // OhraError::Storage("query: ..."). The "query:" prefix lets
        // operators tell pool-acquire failures and interact-panic failures
        // apart from genuine SQL errors at a glance.
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let result: ohara_core::Result<()> = with_conn(&s.pool, |c| {
            c.execute("SELECT * FROM no_such_table", [])?;
            Ok(())
        })
        .await;
        let err = result.expect_err("invalid SQL must surface as an OhraError");
        let msg = err.to_string();
        assert!(
            msg.contains("storage error: query:"),
            "expected a `query:`-stage Storage error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn index_metadata_round_trips_every_initial_component_key() {
        // Plan 13 Task 2.1: write the full set of v0.7-era components
        // and verify round-trip via get_index_metadata. Pins the API
        // shape: caller passes (component, version) pairs, get returns
        // the same map back.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let pairs: Vec<(String, String)> = vec![
            ("schema".into(), "3".into()),
            ("embedding_model".into(), "BAAI/bge-small-en-v1.5".into()),
            ("embedding_dimension".into(), "384".into()),
            ("reranker_model".into(), "bge-reranker-base".into()),
            ("chunker_version".into(), "1".into()),
            ("semantic_text_version".into(), "1".into()),
            ("parser_rust".into(), "1".into()),
            ("parser_python".into(), "1".into()),
            ("parser_java".into(), "1".into()),
            ("parser_kotlin".into(), "1".into()),
        ];
        s.put_index_metadata(&id, &pairs).await.unwrap();
        let stored = s.get_index_metadata(&id).await.unwrap();
        for (k, v) in &pairs {
            assert_eq!(
                stored.components.get(k),
                Some(v),
                "component {k} did not round-trip"
            );
        }
    }

    #[tokio::test]
    async fn put_index_metadata_replacement_is_scoped_to_passed_components() {
        // Plan 13 Task 2.1 Step 2: put_index_metadata MUST NOT delete
        // unrelated component rows — it only updates the components in
        // the call. That property keeps future plans (12, 11) from
        // having to re-write every key just to bump one of theirs.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        s.put_index_metadata(
            &id,
            &[
                ("chunker_version".into(), "1".into()),
                ("parser_rust".into(), "1".into()),
            ],
        )
        .await
        .unwrap();

        // A second put with a different component must leave the prior
        // rows intact.
        s.put_index_metadata(&id, &[("semantic_text_version".into(), "1".into())])
            .await
            .unwrap();

        let stored = s.get_index_metadata(&id).await.unwrap();
        assert_eq!(
            stored.components.get("chunker_version").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            stored.components.get("parser_rust").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            stored
                .components
                .get("semantic_text_version")
                .map(String::as_str),
            Some("1")
        );

        // A third put that updates an existing component must replace
        // its version (no duplicate rows).
        s.put_index_metadata(&id, &[("chunker_version".into(), "2".into())])
            .await
            .unwrap();
        let stored = s.get_index_metadata(&id).await.unwrap();
        assert_eq!(
            stored.components.get("chunker_version").map(String::as_str),
            Some("2")
        );
        // Other components survive unchanged.
        assert_eq!(
            stored.components.get("parser_rust").map(String::as_str),
            Some("1")
        );

        // Row count: one per unique component, no duplicates.
        let pool = s.pool().clone();
        let n: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM index_metadata", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        // Only the 3 components this test wrote — V3's schema-backfill
        // INSERT runs against the `repo` table at migration time, so
        // a repo opened *after* the migration doesn't get a backfilled
        // schema row. Plan 13 Task 2.2 will write `schema` explicitly
        // at the end of every successful index pass.
        assert_eq!(n, 3, "no duplicate rows from upserts");
    }

    #[tokio::test]
    async fn get_index_metadata_returns_empty_for_unknown_repo() {
        // A freshly-opened SqliteStorage with no repo yet returns an
        // empty map (not an error). Callers diagnose this as Unknown,
        // which is the correct user-facing verdict for a brand-new
        // index dir before any open_repo call.
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("never-opened", "/tmp/x");
        let stored = s.get_index_metadata(&id).await.unwrap();
        assert!(
            stored.components.is_empty(),
            "unknown repo must yield an empty StoredIndexMetadata"
        );
    }

    #[tokio::test]
    async fn commit_exists_reports_membership_after_put() {
        // Plan 9 Task 1.2: PK lookup via commit_exists must answer
        // "true" for SHAs we've put_commit'd and "false" for any other
        // SHA. Drives the indexer's resume short-circuit (Task 2.1).
        let (_dir, s, id) = fixture_storage_with_repo().await;
        for sha in &["alpha", "beta"] {
            let cm = CommitMeta {
                commit_sha: (*sha).into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 1,
                message: format!("commit {sha}"),
            };
            s.put_commit(
                &id,
                &CommitRecord {
                    meta: cm,
                    message_emb: vec![0.0; 384],
                },
            )
            .await
            .unwrap();
        }
        assert!(s.commit_exists("alpha").await.unwrap());
        assert!(s.commit_exists("beta").await.unwrap());
        assert!(!s.commit_exists("gamma-never-put").await.unwrap());
    }

    #[tokio::test]
    async fn bm25_hunks_by_semantic_text_finds_section_keywords_not_present_in_diff() {
        // Plan 11 Task 2.2: the semantic-text lane should match
        // structural keywords that the builder injects (file:,
        // change:, language:) which a raw-diff lane couldn't find
        // because they're not in the diff body itself.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        // Manually construct a HunkRecord with a section-formatted
        // semantic_text — bypasses the indexer wiring so the storage
        // contract is exercised in isolation.
        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "fetch: add retry".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
        let rec = HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: "src/fetch.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn fetch() {}".into(),
            },
            diff_emb: vec![0.0_f32; 384],
            semantic_text: "commit: fetch: add retry\n\
                            file: src/fetch.rs\n\
                            language: rust\n\
                            change: added\n\
                            added_lines:\nfn fetch() {}"
                .into(),
            symbols: Vec::new(),
        };
        s.put_hunks(&id, &[rec]).await.unwrap();

        // Query for "language" — present in semantic_text but not in
        // diff_text. Plain BM25-by-diff-text would miss it.
        let semantic_hits = s
            .bm25_hunks_by_semantic_text(&id, "language", 5, None, None)
            .await
            .unwrap();
        assert_eq!(
            semantic_hits.len(),
            1,
            "semantic lane must match the structural keyword"
        );
        let diff_hits = s
            .bm25_hunks_by_text(&id, "language", 5, None, None)
            .await
            .unwrap();
        assert!(
            diff_hits.is_empty(),
            "raw-diff lane must NOT match — confirms the lanes are independent"
        );
    }

    #[tokio::test]
    async fn bm25_hunks_by_semantic_text_still_matches_diff_body_via_added_lines() {
        // Plan 11 Task 2.2 paired test: the semantic-text representation
        // still includes the added-line bodies (via `added_lines:`), so
        // a query that hits the diff body should hit the semantic lane
        // too. Pins that no BM25 vocabulary regressed.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        seed_hunks_with_texts(
            &s,
            &id,
            "c2",
            1_700_000_000,
            &[("a", "+fn retry_with_backoff() {}", Some("rust"))],
        )
        .await;
        let hits = s
            .bm25_hunks_by_semantic_text(&id, "retry", 5, None, None)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "semantic lane must surface the retry hunk via the added_lines body"
        );
    }

    #[tokio::test]
    async fn bm25_hunks_by_historical_symbol_returns_only_hunks_touching_named_symbol() {
        // Plan 11 Task 3.2: a file containing two symbols whose hunks
        // get attributed differently. The historical lane MUST return
        // only the hunk attributed to the queried symbol — that's the
        // problem the file-level lane couldn't solve.
        use ohara_core::types::{AttributionKind, HunkSymbol, SymbolKind};
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "two symbols, two hunks".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
        let attribute = |name: &str| {
            vec![HunkSymbol {
                kind: SymbolKind::Function,
                name: name.into(),
                qualified_name: None,
                attribution: AttributionKind::ExactSpan,
            }]
        };
        let recs = vec![
            HunkRecord {
                hunk: Hunk {
                    commit_sha: "c1".into(),
                    file_path: "src/auth.rs".into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Modified,
                    diff_text: "+    retry();\n".into(),
                },
                diff_emb: vec![0.0_f32; 384],
                semantic_text: "added_lines:\nretry()".into(),
                symbols: attribute("retry_policy"),
            },
            HunkRecord {
                hunk: Hunk {
                    commit_sha: "c1".into(),
                    file_path: "src/auth.rs".into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Modified,
                    diff_text: "+    log();\n".into(),
                },
                diff_emb: vec![0.0_f32; 384],
                semantic_text: "added_lines:\nlog()".into(),
                symbols: attribute("login"),
            },
        ];
        s.put_hunks(&id, &recs).await.unwrap();

        let hits = s
            .bm25_hunks_by_historical_symbol(&id, "retry_policy", 5, None, None)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "historical lane must return ONLY the hunk attributed to the queried symbol, \
             not every hunk in the file containing it"
        );
        // The match must be the retry_policy hunk, not the login hunk.
        assert!(hits[0].hunk.diff_text.contains("retry"));
    }

    #[tokio::test]
    async fn get_hunk_symbols_round_trips_attribution_data() {
        // Plan 11 Task 3.2: get_for_hunk returns the persisted
        // HunkSymbol rows in stable order (exact-span first, then
        // alphabetical by symbol name).
        use ohara_core::types::{AttributionKind, HunkSymbol, SymbolKind};
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta {
            commit_sha: "c1".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_700_000_000,
            message: "m".into(),
        };
        s.put_commit(
            &id,
            &CommitRecord {
                meta: cm,
                message_emb: vec![0.0; 384],
            },
        )
        .await
        .unwrap();
        let symbols = vec![
            HunkSymbol {
                kind: SymbolKind::Function,
                name: "alpha".into(),
                qualified_name: Some("net::alpha".into()),
                attribution: AttributionKind::HunkHeader,
            },
            HunkSymbol {
                kind: SymbolKind::Function,
                name: "bravo".into(),
                qualified_name: None,
                attribution: AttributionKind::ExactSpan,
            },
        ];
        let rec = HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+x\n".into(),
            },
            diff_emb: vec![0.0_f32; 384],
            semantic_text: "+x".into(),
            symbols,
        };
        s.put_hunks(&id, &[rec]).await.unwrap();

        // Get the hunk_id back.
        let pool = s.pool().clone();
        let hunk_id: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| c.query_row("SELECT id FROM hunk LIMIT 1", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        let got = s.get_hunk_symbols(&id, hunk_id).await.unwrap();
        assert_eq!(got.len(), 2);
        // exact_span first.
        assert_eq!(got[0].name, "bravo");
        assert_eq!(got[0].attribution, AttributionKind::ExactSpan);
        assert_eq!(got[1].name, "alpha");
        assert_eq!(got[1].qualified_name.as_deref(), Some("net::alpha"));
    }

    #[tokio::test]
    async fn get_neighboring_file_commits_returns_before_and_after_around_anchor() {
        // Plan 12 Task 3.1 Step 2: anchored at the middle of 5
        // commits, return 2 earlier and 2 later in deterministic
        // timestamp/SHA order. The anchor itself is excluded.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let path = "src/foo.rs";
        for (i, sha) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
            let cm = CommitMeta {
                commit_sha: (*sha).into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 1_700_000_000 + (i as i64) * 1000,
                message: format!("commit {sha}"),
            };
            s.put_commit(
                &id,
                &CommitRecord {
                    meta: cm,
                    message_emb: vec![0.0; 384],
                },
            )
            .await
            .unwrap();
            s.put_hunks(
                &id,
                &[HunkRecord::legacy(
                    Hunk {
                        commit_sha: (*sha).into(),
                        file_path: path.into(),
                        language: Some("rust".into()),
                        change_kind: ChangeKind::Modified,
                        diff_text: format!("+touch {sha}\n"),
                    },
                    vec![0.0_f32; 384],
                )],
            )
            .await
            .unwrap();
        }

        // Anchor on c3; expect c1+c2 (older, newest-first) then c4+c5
        // (newer, oldest-first). Anchor (c3) excluded.
        let neighbors = s
            .get_neighboring_file_commits(&id, path, "c3", 2, 2)
            .await
            .unwrap();
        let shas: Vec<&str> = neighbors
            .iter()
            .map(|(_, cm)| cm.commit_sha.as_str())
            .collect();
        assert_eq!(shas, vec!["c2", "c1", "c4", "c5"]);
        // Each commit touched the file with exactly one hunk.
        for (touched, _) in &neighbors {
            assert_eq!(*touched, 1);
        }
    }

    #[tokio::test]
    async fn get_neighboring_file_commits_returns_empty_when_anchor_not_indexed() {
        // Plan 12 Task 3.1: an unindexed anchor SHA can't be
        // positioned on the timeline; return empty rather than
        // panicking or treating it as anchor=epoch.
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let neighbors = s
            .get_neighboring_file_commits(&id, "src/foo.rs", "no-such-sha", 2, 2)
            .await
            .unwrap();
        assert!(neighbors.is_empty());
    }

    #[tokio::test]
    async fn bm25_hunks_by_text_returns_empty_for_no_match() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        seed_hunks_with_texts(
            &s,
            &id,
            "c1",
            1_700_000_000,
            &[("a", "+fn cooking() {}\n", Some("rust"))],
        )
        .await;

        let hits = s
            .bm25_hunks_by_text(&id, "nonexistentTokenXYZ", 5, None, None)
            .await
            .unwrap();
        assert!(hits.is_empty(), "no FTS match should yield empty Vec");
    }
}
