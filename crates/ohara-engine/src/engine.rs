use crate::handle::RepoHandle;
use ohara_core::embed::RerankProvider;
use ohara_core::types::RepoId;
use ohara_core::EmbeddingProvider;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct RetrievalEngine {
    embedder: Arc<dyn EmbeddingProvider>,
    reranker: Arc<dyn RerankProvider>,
    // Populated in Task A.2 (`open_repo`).
    #[allow(dead_code)]
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
}
