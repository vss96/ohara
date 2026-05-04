//! `RetrievalEngine` — long-lived holder of the embedder, reranker,
//! and per-repo handles.

use crate::error::EngineError;
use crate::handle::RepoHandle;
use ohara_core::embed::RerankProvider;
use ohara_core::types::RepoId;
use ohara_core::EmbeddingProvider;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct RetrievalEngine {
    embedder: Arc<dyn EmbeddingProvider>,
    reranker: Arc<dyn RerankProvider>,
    repos: RwLock<HashMap<RepoId, Arc<RepoHandle>>>,
}

impl RetrievalEngine {
    pub fn new(embedder: Arc<dyn EmbeddingProvider>, reranker: Arc<dyn RerankProvider>) -> Self {
        Self {
            embedder,
            reranker,
            repos: RwLock::new(HashMap::new()),
        }
    }

    pub fn embedder(&self) -> Arc<dyn EmbeddingProvider> {
        self.embedder.clone()
    }

    pub fn reranker(&self) -> Arc<dyn RerankProvider> {
        self.reranker.clone()
    }

    /// Open (or return cached) per-repo handle. Cheap on hit; on miss it
    /// canonicalises the path, derives the repo id, opens SQLite, builds the
    /// retriever, and caches the result.
    pub async fn open_repo(&self, repo_path: impl AsRef<Path>) -> crate::Result<Arc<RepoHandle>> {
        let canonical = std::fs::canonicalize(repo_path.as_ref())
            .map_err(|e| EngineError::Internal(format!("canonicalize: {e}")))?;

        let walker = ohara_git::GitWalker::open(&canonical)
            .map_err(|e| EngineError::Git(format!("open walker: {e}")))?;
        let first = walker
            .first_commit_sha()
            .map_err(|e| EngineError::Git(format!("first_commit_sha: {e}")))?;
        let repo_id = RepoId::from_parts(&first, &canonical.to_string_lossy());

        // Fast path — handle already cached.
        if let Some(existing) = self.repos.read().await.get(&repo_id).cloned() {
            return Ok(existing);
        }

        // Slow path: open storage, build retriever, build blamer.
        tracing::debug!(repo_id = ?repo_id, path = %canonical.display(), "open_repo cache miss");
        let db_path = ohara_core::paths::index_db_path(&repo_id)
            .map_err(|e| EngineError::Internal(format!("index_db_path: {e}")))?;

        let storage: Arc<dyn ohara_core::Storage> = Arc::new(
            ohara_storage::SqliteStorage::open(&db_path)
                .await
                .map_err(|e| EngineError::Storage(format!("open: {e}")))?,
        );

        let retriever = ohara_core::Retriever::new(storage.clone(), self.embedder.clone())
            .with_reranker(self.reranker.clone());

        let blamer = Arc::new(
            ohara_git::Blamer::open(&canonical)
                .map_err(|e| EngineError::Git(format!("blamer open: {e}")))?,
        );

        let handle = Arc::new(RepoHandle {
            repo_id: repo_id.clone(),
            repo_path: canonical,
            storage,
            retriever,
            blamer,
        });

        // Re-check under write lock to avoid double-open race.
        let mut w = self.repos.write().await;
        if let Some(existing) = w.get(&repo_id).cloned() {
            return Ok(existing);
        }
        w.insert(repo_id, handle.clone());
        Ok(handle)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
/// Helpers used by engine.rs tests and reachable from sibling test modules (e.g. server.rs).
pub(crate) mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Serialises tests that mutate `OHARA_HOME` (a process-global env var).
    /// Mirrors the pattern used in `ohara-core/src/paths.rs` and `ohara-cli` tests.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) struct DummyEmbedder;

    #[async_trait::async_trait]
    impl ohara_core::EmbeddingProvider for DummyEmbedder {
        fn dimension(&self) -> usize {
            4
        }

        fn model_id(&self) -> &str {
            "dummy"
        }

        async fn embed_batch(&self, texts: &[String]) -> ohara_core::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
        }
    }

    pub(crate) struct DummyReranker;

    #[async_trait::async_trait]
    impl ohara_core::embed::RerankProvider for DummyReranker {
        async fn rerank(&self, _q: &str, candidates: &[&str]) -> ohara_core::Result<Vec<f32>> {
            Ok(vec![0.0; candidates.len()])
        }
    }

    pub(crate) fn make_test_engine() -> RetrievalEngine {
        RetrievalEngine::new(Arc::new(DummyEmbedder), Arc::new(DummyReranker))
    }

    pub(crate) fn build_test_repo(dir: &Path) {
        use git2::{Repository, Signature};
        let repo = Repository::init(dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn one() {}\n").unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.rs")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
    }

    #[tokio::test]
    async fn find_pattern_returns_empty_hits_on_empty_index() {
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        {
            let _g = env_lock();
            std::env::set_var("OHARA_HOME", ohara_home.path());
        }
        build_test_repo(tmp.path());
        let engine = make_test_engine();
        let q = ohara_core::query::PatternQuery {
            query: "hello".into(),
            k: 5,
            language: None,
            since_unix: None,
            no_rerank: false,
        };
        let out = engine.find_pattern(tmp.path(), q).await.expect("find_pattern");
        assert!(out.hits.is_empty(), "empty index → empty hits, got {:?}", out.hits);
    }

    #[tokio::test]
    async fn open_repo_caches_handle_by_repo_id() {
        // `env_lock` serialises tests that mutate OHARA_HOME (process-global).
        // Drop the guard before the first await to satisfy clippy::await_holding_lock.
        let ohara_home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        {
            let _g = env_lock();
            std::env::set_var("OHARA_HOME", ohara_home.path());
        }
        build_test_repo(tmp.path());

        let engine = make_test_engine();
        let h1 = engine.open_repo(tmp.path()).await.expect("first open");
        let h2 = engine.open_repo(tmp.path()).await.expect("second open");
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "open_repo must return the cached Arc, not a fresh handle"
        );
    }
}
