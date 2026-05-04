use anyhow::{Context, Result};
use ohara_core::embed::RerankProvider;
use ohara_core::index_metadata::{
    CompatibilityStatus, RuntimeIndexMetadata, SCHEMA_VERSION, SEMANTIC_TEXT_VERSION,
};
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
/// owned by `ohara-embed` / `ohara-parse` / `ohara-core`. Mirrored in
/// `ohara_engine::engine::current_runtime_metadata` — kept as a free
/// function here so `ohara-mcp` doesn't depend on engine internals.
pub fn current_runtime_metadata() -> RuntimeIndexMetadata {
    RuntimeIndexMetadata {
        schema_version: SCHEMA_VERSION.to_string(),
        embedding_model: ohara_embed::DEFAULT_MODEL_ID.to_string(),
        embedding_dimension: ohara_embed::DEFAULT_DIM as u32,
        reranker_model: ohara_embed::DEFAULT_RERANKER_ID.to_string(),
        chunker_version: ohara_parse::CHUNKER_VERSION.to_string(),
        semantic_text_version: SEMANTIC_TEXT_VERSION.to_string(),
        parser_versions: ohara_parse::parser_versions(),
    }
}

/// Compose a single hint string from the freshness state and the
/// compatibility verdict. Pulled out so the MCP tests can pin every
/// (freshness, compatibility) combination's wording without standing
/// up a real index.
pub fn compose_hint(
    st: &ohara_core::query::IndexStatus,
    compatibility: &CompatibilityStatus,
) -> Option<String> {
    let freshness_hint = if st.last_indexed_commit.is_none() {
        Some("Index not built. Run `ohara index` in this repo.".to_string())
    } else if st.commits_behind_head > 50 {
        Some(format!(
            "Index is {} commits behind HEAD. Run `ohara index`.",
            st.commits_behind_head
        ))
    } else {
        None
    };
    let compat_hint = match compatibility {
        CompatibilityStatus::Compatible => None,
        CompatibilityStatus::QueryCompatibleNeedsRefresh { reason } => Some(format!(
            "Index is query-compatible but stale ({reason}). Run `ohara index --force` to refresh."
        )),
        CompatibilityStatus::NeedsRebuild { reason } => Some(format!(
            "Index needs rebuild ({reason}). Run `ohara index --rebuild` — find_pattern will refuse to run until then."
        )),
        CompatibilityStatus::Unknown { missing_components } => Some(format!(
            "Index has no recorded metadata for {}. Run `ohara index --force` to record current versions.",
            missing_components.join(", ")
        )),
    };
    match (freshness_hint, compat_hint) {
        (None, None) => None,
        (Some(f), None) => Some(f),
        (None, Some(c)) => Some(c),
        (Some(f), Some(c)) => Some(format!("{f} {c}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::query::IndexStatus;

    fn fresh_status() -> IndexStatus {
        IndexStatus {
            last_indexed_commit: Some("abc".into()),
            commits_behind_head: 0,
            indexed_at: Some("2026-05-03T00:00:00Z".into()),
        }
    }

    #[test]
    fn compose_hint_compatible_fresh_index_returns_none() {
        assert!(compose_hint(&fresh_status(), &CompatibilityStatus::Compatible).is_none());
    }

    #[test]
    fn compose_hint_needs_rebuild_mentions_rebuild_command_and_refusal() {
        let h = compose_hint(
            &fresh_status(),
            &CompatibilityStatus::NeedsRebuild {
                reason: "embedding_dimension mismatch".into(),
            },
        )
        .expect("rebuild verdict must produce a hint");
        assert!(
            h.contains("ohara index --rebuild") && h.contains("refuse"),
            "rebuild hint must point at the command and warn about refusal: {h}"
        );
    }

    #[test]
    fn compose_hint_refresh_recommends_force_not_rebuild() {
        let h = compose_hint(
            &fresh_status(),
            &CompatibilityStatus::QueryCompatibleNeedsRefresh {
                reason: "chunker_version mismatch".into(),
            },
        )
        .expect("refresh verdict must produce a hint");
        assert!(
            h.contains("ohara index --force") && !h.contains("--rebuild"),
            "refresh hint must point at --force, not --rebuild: {h}"
        );
    }

    #[test]
    fn compose_hint_combines_freshness_and_compat_hints() {
        let stale = IndexStatus {
            last_indexed_commit: Some("abc".into()),
            commits_behind_head: 100,
            indexed_at: None,
        };
        let h = compose_hint(
            &stale,
            &CompatibilityStatus::QueryCompatibleNeedsRefresh {
                reason: "chunker_version mismatch".into(),
            },
        )
        .expect("two hints must compose into one");
        assert!(h.contains("100 commits behind") && h.contains("query-compatible"));
    }
}
