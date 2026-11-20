//! Test helpers for embed-stage integration tests.
//!
//! Kept in a sibling module so `embed.rs` stays under 500 lines.
//! This file is compiled only in `#[cfg(test)]` mode.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal in-memory [`crate::storage::StorageCache`]: the embed stage
/// depends only on that seam (issue #98), so this fake implements just
/// the chunk-level embed cache plus trivial blob-cache stubs.
#[derive(Default)]
pub(super) struct InMemoryCacheStorage {
    pub(super) entries: Mutex<HashMap<(crate::types::ContentHash, String), Vec<f32>>>,
}

#[async_trait]
impl crate::storage::StorageCache for InMemoryCacheStorage {
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
}
