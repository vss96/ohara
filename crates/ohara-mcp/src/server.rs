use anyhow::{Context, Result};
use ohara_core::embed::RerankProvider;
use ohara_core::index_metadata::RuntimeIndexMetadata;
use ohara_core::EmbeddingProvider;
use ohara_engine::RetrievalEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct OharaServer {
    pub repo_path: PathBuf,
    pub engine: Arc<RetrievalEngine>,
}

impl OharaServer {
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;

        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(
            ohara_core::perf_trace::timed_phase(
                "embed_load",
                tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new),
            )
            .await??,
        );
        // Plan 3: attach the cross-encoder reranker by default. Per-call
        // opt-out is the MCP `no_rerank: true` flag, plumbed through
        // `PatternQuery`. First boot downloads ~110 MB for bge-reranker-base.
        let reranker: Arc<dyn RerankProvider> = Arc::new(
            ohara_core::perf_trace::timed_phase(
                "rerank_load",
                tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new),
            )
            .await??,
        );

        let engine = Arc::new(RetrievalEngine::new(embedder, reranker));
        // Warm the per-repo handle so the first MCP call doesn't pay
        // the cold-open cost of deriving the repo-id and opening SQLite.
        engine
            .open_repo(&canonical)
            .await
            .context("warm repo handle")?;

        Ok(Self {
            repo_path: canonical,
            engine,
        })
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }
}

/// Build the runtime compatibility expectation from the constants
/// owned by `ohara-embed` / `ohara-parse`. Delegates to the canonical
/// `ohara_core::index_metadata::runtime_metadata_from` helper.
pub fn current_runtime_metadata() -> RuntimeIndexMetadata {
    ohara_core::index_metadata::runtime_metadata_from(
        ohara_embed::DEFAULT_MODEL_ID,
        ohara_embed::DEFAULT_DIM as u32,
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
        "semantic",
    )
}

/// Compose a single hint string from the freshness state and the
/// compatibility verdict. Delegates to the canonical
/// `ohara_core::index_metadata::compose_hint` so the wording is kept
/// in one place.
pub fn compose_hint(
    st: &ohara_core::query::IndexStatus,
    compatibility: &ohara_core::index_metadata::CompatibilityStatus,
) -> Option<String> {
    ohara_core::index_metadata::compose_hint(st, compatibility)
}

// Tests for compose_hint and compose_hint wording are now in
// ohara-core::index_metadata::tests to avoid duplication.
// MCP-layer integration tests live in crates/ohara-mcp/tests/.
