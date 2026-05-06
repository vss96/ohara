//! Test helpers for embed-stage integration tests.
//!
//! Kept in a sibling module so `embed.rs` stays under 500 lines.
//! This file is compiled only in `#[cfg(test)]` mode.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal in-memory `Storage` that only implements the chunk-level embed
/// cache (`embed_cache_get_many` / `embed_cache_put_many`). All other
/// methods are trivial stubs returning empty / default values — sufficient
/// for unit tests that only exercise the cache hit/miss path.
#[derive(Default)]
pub(super) struct InMemoryCacheStorage {
    pub(super) entries: Mutex<HashMap<(crate::types::ContentHash, String), Vec<f32>>>,
}

#[async_trait]
impl crate::Storage for InMemoryCacheStorage {
    async fn open_repo(
        &self,
        _repo_id: &crate::types::RepoId,
        _path: &str,
        _first_commit_sha: &str,
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn get_index_status(
        &self,
        _repo_id: &crate::types::RepoId,
    ) -> crate::Result<crate::query::IndexStatus> {
        Ok(crate::query::IndexStatus::default())
    }

    async fn set_last_indexed_commit(
        &self,
        _repo_id: &crate::types::RepoId,
        _sha: &str,
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn put_commit(
        &self,
        _repo_id: &crate::types::RepoId,
        _record: &crate::storage::CommitRecord,
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn commit_exists(&self, _sha: &str) -> crate::Result<bool> {
        Ok(false)
    }

    async fn put_hunks(
        &self,
        _repo_id: &crate::types::RepoId,
        _records: &[crate::storage::HunkRecord],
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn put_head_symbols(
        &self,
        _repo_id: &crate::types::RepoId,
        _symbols: &[crate::types::Symbol],
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn clear_head_symbols(&self, _repo_id: &crate::types::RepoId) -> crate::Result<()> {
        Ok(())
    }

    async fn knn_hunks(
        &self,
        _repo_id: &crate::types::RepoId,
        _query_emb: &[f32],
        _k: u8,
        _language: Option<&str>,
        _since_unix: Option<i64>,
    ) -> crate::Result<Vec<crate::storage::HunkHit>> {
        Ok(vec![])
    }

    async fn bm25_hunks_by_text(
        &self,
        _repo_id: &crate::types::RepoId,
        _query: &str,
        _k: u8,
        _language: Option<&str>,
        _since_unix: Option<i64>,
    ) -> crate::Result<Vec<crate::storage::HunkHit>> {
        Ok(vec![])
    }

    async fn bm25_hunks_by_semantic_text(
        &self,
        _repo_id: &crate::types::RepoId,
        _query: &str,
        _k: u8,
        _language: Option<&str>,
        _since_unix: Option<i64>,
    ) -> crate::Result<Vec<crate::storage::HunkHit>> {
        Ok(vec![])
    }

    async fn bm25_hunks_by_symbol_name(
        &self,
        _repo_id: &crate::types::RepoId,
        _query: &str,
        _k: u8,
        _language: Option<&str>,
        _since_unix: Option<i64>,
    ) -> crate::Result<Vec<crate::storage::HunkHit>> {
        Ok(vec![])
    }

    async fn bm25_hunks_by_historical_symbol(
        &self,
        _repo_id: &crate::types::RepoId,
        _query: &str,
        _k: u8,
        _language: Option<&str>,
        _since_unix: Option<i64>,
    ) -> crate::Result<Vec<crate::storage::HunkHit>> {
        Ok(vec![])
    }

    async fn get_hunk_symbols(
        &self,
        _repo_id: &crate::types::RepoId,
        _hunk_id: crate::storage::HunkId,
    ) -> crate::Result<Vec<crate::types::HunkSymbol>> {
        Ok(vec![])
    }

    async fn get_hunk_symbols_batch(
        &self,
        _repo_id: &crate::types::RepoId,
        hunk_ids: &[crate::storage::HunkId],
    ) -> crate::Result<HashMap<crate::storage::HunkId, Vec<crate::types::HunkSymbol>>> {
        Ok(hunk_ids.iter().map(|&id| (id, vec![])).collect())
    }

    async fn blob_was_seen(&self, _blob_sha: &str, _embedding_model: &str) -> crate::Result<bool> {
        Ok(false)
    }

    async fn record_blob_seen(&self, _blob_sha: &str, _embedding_model: &str) -> crate::Result<()> {
        Ok(())
    }

    async fn embed_cache_get_many(
        &self,
        hashes: &[crate::types::ContentHash],
        embed_model: &str,
    ) -> crate::Result<HashMap<crate::types::ContentHash, Vec<f32>>> {
        let entries = self.entries.lock().unwrap();
        let mut out = HashMap::new();
        for h in hashes {
            if let Some(v) = entries.get(&(h.clone(), embed_model.to_owned())) {
                out.insert(h.clone(), v.clone());
            }
        }
        Ok(out)
    }

    async fn embed_cache_put_many(
        &self,
        entries_in: &[(crate::types::ContentHash, Vec<f32>)],
        embed_model: &str,
    ) -> crate::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        for (h, v) in entries_in {
            entries.insert((h.clone(), embed_model.to_owned()), v.clone());
        }
        Ok(())
    }

    async fn get_commit(
        &self,
        _repo_id: &crate::types::RepoId,
        _sha: &str,
    ) -> crate::Result<Option<crate::types::CommitMeta>> {
        Ok(None)
    }

    async fn get_hunks_for_file_in_commit(
        &self,
        _repo_id: &crate::types::RepoId,
        _sha: &str,
        _file_path: &str,
    ) -> crate::Result<Vec<crate::types::Hunk>> {
        Ok(vec![])
    }

    async fn get_neighboring_file_commits(
        &self,
        _repo_id: &crate::types::RepoId,
        _file_path: &str,
        _anchor_sha: &str,
        _limit_before: u8,
        _limit_after: u8,
    ) -> crate::Result<Vec<(u32, crate::types::CommitMeta)>> {
        Ok(vec![])
    }

    async fn get_index_metadata(
        &self,
        _repo_id: &crate::types::RepoId,
    ) -> crate::Result<crate::index_metadata::StoredIndexMetadata> {
        Ok(crate::index_metadata::StoredIndexMetadata::default())
    }

    async fn put_index_metadata(
        &self,
        _repo_id: &crate::types::RepoId,
        _components: &[(String, String)],
    ) -> crate::Result<()> {
        Ok(())
    }
}
