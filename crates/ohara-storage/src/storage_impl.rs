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
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
        .interact(f)
        .await
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
        .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
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
        with_conn(&self.pool, move |c| crate::commit::put(c, &rec)).await
    }
    async fn put_hunks(&self, _repo_id: &RepoId, records: &[HunkRecord]) -> CoreResult<()> {
        let recs = records.to_vec();
        with_conn(&self.pool, move |c| crate::hunk::put_many(c, &recs)).await
    }

    async fn put_head_symbols(&self, _repo_id: &RepoId, symbols: &[Symbol]) -> CoreResult<()> {
        // Plan 3 / Track A: persist symbols + mirror into fts_symbol_name so
        // the BM25-by-symbol-name lane has rows to match against. Track C
        // will later wire `Symbol::sibling_names` through this path.
        let syms = symbols.to_vec();
        with_conn(&self.pool, move |c| crate::symbol::put_many(c, &syms)).await
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
            crate::hunk::knn(c, &qe, k, lang.as_deref(), since_unix)
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
            crate::hunk::bm25_by_text(c, &q, k, lang.as_deref(), since_unix)
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
            crate::symbol::bm25_by_name(c, &q, k, lang.as_deref(), since_unix)
        })
        .await
    }

    async fn blob_was_seen(&self, blob_sha: &str, model: &str) -> CoreResult<bool> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        with_conn(&self.pool, move |c| {
            crate::blob_cache::was_seen(c, &blob, &m)
        })
        .await
    }

    async fn record_blob_seen(&self, blob_sha: &str, model: &str) -> CoreResult<()> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        with_conn(&self.pool, move |c| crate::blob_cache::record(c, &blob, &m)).await
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
            sha: "abc".into(),
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
    async fn put_commit_embedding_round_trips_through_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite"))
            .await
            .unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();

        let original: Vec<f32> = (0..384).map(|i| (i as f32) * 0.001 - 0.2).collect();
        let cm = CommitMeta {
            sha: "rt".into(),
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
                Ok::<_, rusqlite::Error>(crate::vec_codec::bytes_to_vec(&bytes))
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
            sha: "c1".into(),
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

        let h = HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn x() {}\n".into(),
            },
            diff_emb: vec![0.5f32; 384],
        };
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
            sha: "c1".into(),
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

        let mk_hunk = |emb_val: f32, name: &str| HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: format!("src/{name}.rs"),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: format!("+fn {name}() {{}}\n"),
            },
            diff_emb: vec![emb_val; 384],
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
            sha: "c1".into(),
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
        let mk = |val: f32, name: &str| HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: format!("{name}.rs"),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: format!("+{name}"),
            },
            diff_emb: vec![val; 384],
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
            sha: commit_sha.into(),
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
            .map(|(name, diff, lang)| HunkRecord {
                hunk: Hunk {
                    commit_sha: commit_sha.into(),
                    file_path: format!("src/{name}.rs"),
                    language: lang.map(str::to_string),
                    change_kind: ChangeKind::Added,
                    diff_text: (*diff).to_string(),
                },
                diff_emb: vec![0.0f32; 384],
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
        assert_eq!(hits[0].commit.sha, "new");
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
