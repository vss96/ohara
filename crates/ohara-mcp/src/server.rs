use anyhow::{Context, Result};
use ohara_core::embed::RerankProvider;
use ohara_core::index_metadata::{
    CompatibilityStatus, RuntimeIndexMetadata, SCHEMA_VERSION, SEMANTIC_TEXT_VERSION,
};
use ohara_core::perf_trace::timed_phase;
use ohara_core::types::RepoId;
use ohara_core::{EmbeddingProvider, Retriever, Storage};
use ohara_git::Blamer;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct OharaServer {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,
    pub storage: Arc<dyn Storage>,
    pub retriever: Retriever,
    /// Plan 5: blame source backing the `explain_change` tool. One per
    /// session; reuses the underlying `git2::Repository` via
    /// `Arc<Mutex<Repository>>` (set up inside `Blamer::open`).
    pub blamer: Arc<Blamer>,
}

impl OharaServer {
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        let walker = ohara_git::GitWalker::open(&canonical).context("open repo")?;
        let first_commit = walker.first_commit_sha()?;
        let repo_id = RepoId::from_parts(&first_commit, &canonical.to_string_lossy());

        let db_path = ohara_core::paths::index_db_path(&repo_id)?;

        let storage: Arc<dyn Storage> = Arc::new(
            timed_phase("storage_open", ohara_storage::SqliteStorage::open(&db_path)).await?,
        );
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(
            timed_phase(
                "embed_load",
                tokio::task::spawn_blocking(ohara_embed::FastEmbedProvider::new),
            )
            .await??,
        );
        // Plan 3: attach the cross-encoder reranker by default. Per-call
        // opt-out is the MCP `no_rerank: true` flag, plumbed through
        // `PatternQuery`. First boot downloads ~110 MB for bge-reranker-base.
        let reranker: Arc<dyn RerankProvider> = Arc::new(
            timed_phase(
                "rerank_load",
                tokio::task::spawn_blocking(ohara_embed::FastEmbedReranker::new),
            )
            .await??,
        );
        let retriever = Retriever::new(storage.clone(), embedder.clone()).with_reranker(reranker);

        // Plan 5: blame source for `explain_change`. Reads from the same
        // workdir; no model download or async work needed.
        let blamer = Arc::new(
            timed_phase("blamer_open", async { Blamer::open(&canonical) })
                .await
                .context("open blamer")?,
        );

        Ok(Self {
            repo_id,
            repo_path: canonical,
            storage,
            retriever,
            blamer,
        })
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }

    pub async fn index_status_meta(&self) -> Result<ohara_core::query::ResponseMeta> {
        let behind = ohara_git::GitCommitsBehind::open(&self.repo_path)?;
        let st =
            ohara_core::query::compute_index_status(self.storage.as_ref(), &self.repo_id, &behind)
                .await?;
        let compatibility = self.compatibility_status().await?;
        let hint = compose_hint(&st, &compatibility);
        Ok(ohara_core::query::ResponseMeta {
            index_status: st,
            hint,
            compatibility: Some(compatibility),
        })
    }

    /// Plan 13: build the runtime compatibility expectation and assess
    /// it against what's stored in the opened index. Used by both MCP
    /// tools (find_pattern fails early on NeedsRebuild; both surface
    /// the verdict in `_meta`).
    pub async fn compatibility_status(&self) -> Result<CompatibilityStatus> {
        let runtime = current_runtime_metadata();
        let stored = self.storage.get_index_metadata(&self.repo_id).await?;
        Ok(CompatibilityStatus::assess(&runtime, &stored))
    }
}

/// Build the runtime compatibility expectation from the constants
/// owned by `ohara-embed` / `ohara-parse` / `ohara-core`. Mirrored in
/// `ohara_cli::commands::status::current_runtime_metadata` — kept as
/// a free function in each crate (rather than a shared helper) so
/// neither binary has to depend on the other.
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
