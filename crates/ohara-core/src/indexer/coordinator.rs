//! Coordinator: drives the 5-stage pipeline per commit.

use crate::indexer::stages::{
    attribute::AttributeStage, commit_walk::CommitWalkStage,
    embed::EmbedStage, hunk_chunk::HunkChunkStage, persist::PersistStage,
};
use crate::indexer::stages::attribute::AttributedHunk;
use crate::indexer::stages::commit_walk::CommitWatermark;
use crate::indexer::{AtomicSymbolExtractor, CommitSource, NullAtomicSymbolExtractor, SymbolSource};
use crate::types::{CommitMeta, RepoId};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;

/// Drives the 5-stage indexer pipeline per commit.
///
/// The coordinator:
/// - Queries `Storage::get_index_status` once per run to build the
///   resume watermark.
/// - Filters `CommitWalkStage` output to skip already-indexed commits.
/// - Orchestrates stages 2-5 per commit.
/// - Does NOT hold per-stage state — stages are constructed fresh per
///   `run` call so the coordinator is safe to re-use across runs.
pub struct Coordinator {
    storage: Arc<dyn Storage + Send + Sync>,
    embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    embed_batch: usize,
}

impl Coordinator {
    /// Construct a coordinator with the default `embed_batch` of 32.
    pub fn new(
        storage: Arc<dyn Storage + Send + Sync>,
        embedder: Arc<dyn EmbeddingProvider + Send + Sync>,
    ) -> Self {
        Self {
            storage,
            embedder,
            embed_batch: 32,
        }
    }

    /// Override the embed stage's batch size.
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Run the full 5-stage pipeline for all commits in `source` that
    /// follow the resume watermark.
    pub async fn run(
        &self,
        repo: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<()> {
        // Stage 0: determine resume watermark from index status.
        let status = self.storage.get_index_status(repo).await?;
        let watermark = status
            .last_indexed_commit
            .as_deref()
            .map(CommitWatermark::new);

        // Stage 1: commit walk.
        let commits = CommitWalkStage::run(commit_source, watermark.as_ref()).await?;

        let embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch);

        // Null extractor — binaries wire the real tree-sitter extractor
        // via `Indexer::with_atomic_symbol_extractor`.
        let extractor = NullAtomicSymbolExtractor;

        for commit in &commits {
            // Skip commits that are already indexed.
            if self.storage.commit_exists(&commit.commit_sha).await? {
                tracing::debug!(sha = %commit.commit_sha, "plan-19: skipping already-indexed commit");
                continue;
            }
            self.run_commit(
                repo,
                commit,
                commit_source,
                symbol_source,
                &embed_stage,
                &extractor,
            )
            .await?;
        }
        Ok(())
    }

    /// Run stages 2-5 for a single commit.
    async fn run_commit(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        _embed_stage: &EmbedStage,
        extractor: &dyn AtomicSymbolExtractor,
    ) -> Result<()> {
        // Stage 2: hunk chunk.
        let records = HunkChunkStage::run(commit_source, commit).await?;

        // Stage 3: attribute.
        let attributed = AttributeStage::run(
            &records,
            &commit.commit_sha,
            commit_source,
            symbol_source,
            extractor,
        )
        .await?;

        // Stages 4-5 share a helper so they can be tested in isolation.
        self.run_from_attributed(repo, commit, attributed).await
    }

    /// Run stages 4 (embed) and 5 (persist) given pre-built
    /// `AttributedHunk` values.
    ///
    /// This entry point enables "resume from after attribute stage":
    /// a caller can construct `Vec<AttributedHunk>` directly (e.g.
    /// from a checkpoint) and drive only the downstream stages.
    pub async fn run_from_attributed(
        &self,
        repo: &RepoId,
        commit: &CommitMeta,
        attributed: Vec<AttributedHunk>,
    ) -> Result<()> {
        let embed_stage = EmbedStage::new(self.embedder.clone())
            .with_embed_batch(self.embed_batch);

        // Stage 4: embed.
        let embed_output = embed_stage.run(&commit.message, &attributed).await?;

        // Stage 5: persist.
        PersistStage::run(repo, commit, embed_output, self.storage.as_ref()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::stages::commit_walk::CommitWatermark;
    use crate::index_metadata::StoredIndexMetadata;
    use crate::query::IndexStatus;
    use crate::storage::{CommitRecord, HunkRecord as StorageHunkRecord, HunkHit, HunkId, StorageMetricsSnapshot};
    use crate::types::{CommitMeta, Hunk, HunkSymbol, RepoId, Symbol};
    use crate::{EmbeddingProvider, OhraError, Result, Storage};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    // --- Minimal fakes reused across coordinator tests ---

    struct SingleCommitSource {
        sha: String,
        hunks: Vec<Hunk>,
    }

    #[async_trait]
    impl crate::indexer::CommitSource for SingleCommitSource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
            Ok(vec![CommitMeta {
                commit_sha: self.sha.clone(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 1_000_000,
                message: "add feature".into(),
            }])
        }
        async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
            Ok(self.hunks.clone())
        }
    }

    struct NoopSymbolSource;
    #[async_trait]
    impl crate::indexer::SymbolSource for NoopSymbolSource {
        async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
            Ok(vec![])
        }
    }

    struct ZeroEmbedder { dim: usize }
    #[async_trait]
    impl EmbeddingProvider for ZeroEmbedder {
        fn dimension(&self) -> usize { self.dim }
        fn model_id(&self) -> &str { "zero" }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    #[derive(Default)]
    struct SpyStorage {
        put_commit_calls: Mutex<Vec<String>>,
        put_hunk_totals: Mutex<Vec<usize>>,
        watermark: Mutex<Option<String>>,
        seen_commits: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Storage for SpyStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            Ok(IndexStatus {
                last_indexed_commit: self.watermark.lock().unwrap().clone(),
                commits_behind_head: 0,
                indexed_at: None,
            })
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, meta: &CommitRecord) -> Result<()> {
            self.put_commit_calls.lock().unwrap().push(meta.meta.commit_sha.clone());
            self.seen_commits.lock().unwrap().push(meta.meta.commit_sha.clone());
            Ok(())
        }
        async fn commit_exists(&self, sha: &str) -> Result<bool> {
            // If we have a watermark matching sha, consider it already indexed.
            let wm = self.watermark.lock().unwrap().clone();
            Ok(wm.as_deref() == Some(sha) || self.seen_commits.lock().unwrap().contains(&sha.to_string()))
        }
        async fn put_hunks(&self, _: &RepoId, rows: &[StorageHunkRecord]) -> Result<()> {
            self.put_hunk_totals.lock().unwrap().push(rows.len());
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> { Ok(()) }
        async fn clear_head_symbols(&self, _: &RepoId) -> Result<()> { Ok(()) }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_semantic_text(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_symbol_name(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn bm25_hunks_by_historical_symbol(&self, _: &RepoId, _: &str, _: u8, _: Option<&str>, _: Option<i64>) -> Result<Vec<HunkHit>> { Ok(vec![]) }
        async fn get_hunk_symbols(&self, _: &RepoId, _: HunkId) -> Result<Vec<HunkSymbol>> { Ok(vec![]) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn get_commit(&self, _: &RepoId, _: &str) -> Result<Option<CommitMeta>> { Ok(None) }
        async fn get_hunks_for_file_in_commit(&self, _: &RepoId, _: &str, _: &str) -> Result<Vec<Hunk>> { Ok(vec![]) }
        async fn get_neighboring_file_commits(&self, _: &RepoId, _: &str, _: &str, _: u8, _: u8) -> Result<Vec<(u32, CommitMeta)>> { Ok(vec![]) }
        async fn get_index_metadata(&self, _: &RepoId) -> Result<StoredIndexMetadata> { Ok(StoredIndexMetadata::default()) }
        async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> { Ok(()) }
    }

    fn hunk(sha: &str) -> Hunk {
        use crate::types::ChangeKind;
        Hunk {
            commit_sha: sha.into(),
            file_path: "src/lib.rs".into(),
            language: None,
            change_kind: ChangeKind::Added,
            diff_text: "+fn x() {}\n".into(),
        }
    }

    #[tokio::test]
    async fn coordinator_indexes_single_commit_end_to_end() {
        let storage = Arc::new(SpyStorage::default());
        let coordinator = Coordinator::new(
            storage.clone(),
            Arc::new(ZeroEmbedder { dim: 4 }),
        );
        let repo = RepoId::from_parts("sha", "/repo");
        let source = SingleCommitSource {
            sha: "abc".into(),
            hunks: vec![hunk("abc")],
        };
        coordinator
            .run(&repo, &source, &NoopSymbolSource)
            .await
            .unwrap();

        assert_eq!(
            *storage.put_commit_calls.lock().unwrap(),
            vec!["abc"],
            "coordinator must persist exactly one commit"
        );
        assert_eq!(
            *storage.put_hunk_totals.lock().unwrap(),
            vec![1],
            "coordinator must persist one hunk"
        );
    }

    #[tokio::test]
    async fn coordinator_resumes_skipping_already_indexed_commit() {
        // Watermark is already at "abc" — the commit source still
        // returns "abc" but the coordinator must skip it.
        let storage = Arc::new(SpyStorage {
            watermark: Mutex::new(Some("abc".into())),
            ..Default::default()
        });
        let coordinator = Coordinator::new(
            storage.clone(),
            Arc::new(ZeroEmbedder { dim: 4 }),
        );
        let repo = RepoId::from_parts("sha", "/repo");
        let source = SingleCommitSource {
            sha: "abc".into(),
            hunks: vec![hunk("abc")],
        };
        coordinator
            .run(&repo, &source, &NoopSymbolSource)
            .await
            .unwrap();

        assert!(
            storage.put_commit_calls.lock().unwrap().is_empty(),
            "coordinator must not re-index an already-indexed commit"
        );
    }

    #[tokio::test]
    async fn coordinator_resume_from_attributed_hunks_directly() {
        use crate::indexer::stages::attribute::AttributedHunk;
        use crate::indexer::stages::hunk_chunk::HunkRecord;

        let storage = Arc::new(SpyStorage::default());
        let embedder = Arc::new(ZeroEmbedder { dim: 4 });
        let coordinator = Coordinator::new(storage.clone(), embedder);

        let repo = RepoId::from_parts("sha", "/repo");
        let commit = CommitMeta {
            commit_sha: "abc".into(),
            parent_sha: None,
            is_merge: false,
            author: None,
            ts: 1_000_000,
            message: "add feature".into(),
        };
        let attributed = vec![AttributedHunk {
            record: HunkRecord {
                commit_sha: "abc".into(),
                file_path: "src/lib.rs".into(),
                diff_text: "+fn x() {}\n".into(),
                semantic_text: "fn x() {}".into(),
                source_hunk: Hunk::default(),
            },
            symbols: None,
            attributed_semantic_text: None,
        }];

        coordinator
            .run_from_attributed(&repo, &commit, attributed)
            .await
            .unwrap();

        assert_eq!(
            *storage.put_commit_calls.lock().unwrap(),
            vec!["abc"],
            "partial-pipeline run must still persist the commit"
        );
        assert_eq!(
            *storage.put_hunk_totals.lock().unwrap(),
            vec![1],
            "partial-pipeline run must persist the hunk"
        );
    }
}
