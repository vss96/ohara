//! Plan 13 — index compatibility model.
//!
//! `RuntimeIndexMetadata` describes what the current binary expects to
//! find inside an opened index (embedding model + dimension, reranker,
//! chunker version, semantic-text version, parser versions per
//! language, schema). `StoredIndexMetadata` is the snapshot read from
//! the per-component `index_metadata` rows. `CompatibilityStatus`
//! reports the verdict so callers (CLI status, MCP `_meta`, future
//! retrieval gates) can decide between "fine", "refresh recommended",
//! "rebuild required", and "no idea — older index, missing rows".
//!
//! Dimension or embedding-model mismatches force `NeedsRebuild` because
//! they invalidate vec-side KNN. Derived components (chunker, parsers,
//! semantic text, reranker) only force `QueryCompatibleNeedsRefresh`:
//! an `ohara index --force` repopulates them without re-embedding the
//! whole history.

use crate::query::IndexStatus;
use crate::EmbeddingProvider;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Refinery schema version this binary expects. Bumped in lock-step
/// with new `crates/ohara-storage/migrations/V*.sql` files.
pub const SCHEMA_VERSION: &str = "4";

/// Semantic-text builder version (plan 11). `"1"` is the
/// section-structured builder that lands in plan 11 Task 2.1
/// (commit / file / language / symbols / change / added_lines).
pub const SEMANTIC_TEXT_VERSION: &str = "1";

/// What the current binary expects the index to have been built with.
/// Built fresh on every CLI / MCP invocation from the actual
/// embedder, parser, chunker, etc. handles in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIndexMetadata {
    /// Refinery schema version, as a string (e.g. "3").
    pub schema_version: String,
    /// Stable model id from `EmbeddingProvider::model_id()`.
    pub embedding_model: String,
    /// Vector dimension from `EmbeddingProvider::dimension()`.
    pub embedding_dimension: u32,
    /// Reranker model id (e.g. `"bge-reranker-base"`).
    pub reranker_model: String,
    /// AST sibling-merge chunker version. Bumped when chunker output
    /// semantics change.
    pub chunker_version: String,
    /// Semantic-text builder version (plan 11). Bumped when the
    /// hunk-text shape fed to the embedder / FTS lanes changes.
    pub semantic_text_version: String,
    /// `language -> parser_version` for every language the binary
    /// can index. Stored under `parser_<language>` component keys.
    pub parser_versions: BTreeMap<String, String>,
}

impl RuntimeIndexMetadata {
    /// Build the current runtime metadata from the live embedder
    /// handle plus caller-supplied derived-component versions. The
    /// embedder is the only source of truth for `embedding_model` and
    /// `embedding_dimension`; the rest comes from constants owned by
    /// the crate that hosts the relevant code.
    ///
    /// Callers in the CLI / MCP wire `chunker_version`,
    /// `parser_versions` from `ohara_parse::CHUNKER_VERSION` /
    /// `ohara_parse::parser_versions()`, and `reranker_model` from
    /// `FastEmbedReranker::model_id()`.
    pub fn current(
        embedder: &dyn EmbeddingProvider,
        reranker_model: impl Into<String>,
        chunker_version: impl Into<String>,
        parser_versions: BTreeMap<String, String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            embedding_model: embedder.model_id().to_string(),
            embedding_dimension: u32::try_from(embedder.dimension()).unwrap_or(u32::MAX),
            reranker_model: reranker_model.into(),
            chunker_version: chunker_version.into(),
            semantic_text_version: SEMANTIC_TEXT_VERSION.to_string(),
            parser_versions,
        }
    }

    /// Flatten this metadata into the `(component, version)` pair list
    /// expected by `Storage::put_index_metadata`. Field order is
    /// stable across runs (BTreeMap iteration + a fixed prefix).
    pub fn to_storage_components(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = vec![
            ("schema".into(), self.schema_version.clone()),
            ("embedding_model".into(), self.embedding_model.clone()),
            (
                "embedding_dimension".into(),
                self.embedding_dimension.to_string(),
            ),
            ("reranker_model".into(), self.reranker_model.clone()),
            ("chunker_version".into(), self.chunker_version.clone()),
            (
                "semantic_text_version".into(),
                self.semantic_text_version.clone(),
            ),
        ];
        for (lang, ver) in &self.parser_versions {
            out.push((format!("parser_{lang}"), ver.clone()));
        }
        out
    }
}

/// Per-component snapshot read from the `index_metadata` table.
/// `components` maps the row's `component` column to its `version`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredIndexMetadata {
    pub components: BTreeMap<String, String>,
}

/// Compatibility verdict between `RuntimeIndexMetadata` and
/// `StoredIndexMetadata`. Each non-`Compatible` variant carries the
/// offending component(s) so the CLI / MCP layer can report something
/// actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompatibilityStatus {
    /// Every recorded component matches the runtime expectation.
    Compatible,
    /// Vector-side state is fine, but a derived component (chunker,
    /// parser, semantic text, reranker) is on a stale version. KNN
    /// still works; `ohara index --force` repopulates the derived rows.
    QueryCompatibleNeedsRefresh { reason: String },
    /// A vector-affecting component (embedding model / dimension /
    /// schema) differs. Continuing would produce wrong KNN results;
    /// `ohara index --rebuild` is the only safe answer.
    NeedsRebuild { reason: String },
    /// At least one expected component has no stored row. Could be a
    /// pre-v0.7 index that hasn't been refreshed since the V3
    /// migration ran, or a freshly migrated one before any index pass
    /// wrote new metadata.
    Unknown { missing_components: Vec<String> },
}

impl CompatibilityStatus {
    /// Compare `runtime` against `stored` and produce a verdict.
    ///
    /// Order of checks:
    /// 1. Vector-affecting (embedding model + dimension, schema) —
    ///    mismatch returns `NeedsRebuild`. Missing returns `Unknown`.
    /// 2. Derived (chunker, semantic text, reranker, per-language
    ///    parsers) — mismatch returns `QueryCompatibleNeedsRefresh`.
    ///    Missing rows accumulate into the `Unknown` variant rather
    ///    than masking each other.
    ///
    /// "Vector-affecting first" is intentional: a `NeedsRebuild`
    /// diagnosis dominates a refresh recommendation, and a missing
    /// embedding row is a stronger signal than a missing chunker row.
    pub fn assess(runtime: &RuntimeIndexMetadata, stored: &StoredIndexMetadata) -> Self {
        // 1. Vector-affecting components (mismatch -> rebuild). Only
        //    embedding-side mismatches go here: a different embedder or
        //    a different vector dimension means stored KNN vectors would
        //    be wrong against a query embedded by the current binary.
        //    Schema lives in the derived bucket below — refinery
        //    migrations are append-only and additive, so a schema bump
        //    needs a refresh to populate the new columns / tables, not
        //    a vector rebuild.
        let vector_affecting: [(&str, String); 2] = [
            ("embedding_model", runtime.embedding_model.clone()),
            (
                "embedding_dimension",
                runtime.embedding_dimension.to_string(),
            ),
        ];
        let mut missing: Vec<String> = Vec::new();
        for (key, expected) in &vector_affecting {
            match stored.components.get(*key) {
                Some(actual) if actual != expected => {
                    return CompatibilityStatus::NeedsRebuild {
                        reason: format!(
                            "{key} mismatch (index has \"{actual}\", binary expects \"{expected}\")"
                        ),
                    };
                }
                None => missing.push((*key).to_string()),
                _ => {}
            }
        }

        // 2. Derived components (mismatch -> refresh).
        let mut derived: Vec<(String, String)> = vec![
            ("schema".to_string(), runtime.schema_version.clone()),
            (
                "chunker_version".to_string(),
                runtime.chunker_version.clone(),
            ),
            (
                "semantic_text_version".to_string(),
                runtime.semantic_text_version.clone(),
            ),
            ("reranker_model".to_string(), runtime.reranker_model.clone()),
        ];
        // BTreeMap iteration is ordered, so this is deterministic.
        for (lang, version) in &runtime.parser_versions {
            derived.push((format!("parser_{lang}"), version.clone()));
        }
        for (key, expected) in &derived {
            match stored.components.get(key) {
                Some(actual) if actual != expected => {
                    return CompatibilityStatus::QueryCompatibleNeedsRefresh {
                        reason: format!(
                            "{key} mismatch (index has \"{actual}\", binary expects \"{expected}\")"
                        ),
                    };
                }
                None => missing.push(key.clone()),
                _ => {}
            }
        }

        if missing.is_empty() {
            CompatibilityStatus::Compatible
        } else {
            CompatibilityStatus::Unknown {
                missing_components: missing,
            }
        }
    }
}

/// Build a [`RuntimeIndexMetadata`] from caller-supplied constant strings.
///
/// `ohara-core` MUST NOT depend on `ohara-embed` or `ohara-parse`, so it
/// cannot read the real constants itself. Callers in `ohara-engine` and
/// `ohara-mcp` pass those constants in and receive the fully-constructed
/// struct back. This removes the duplicated construction logic from both
/// crates while keeping the dependency rule intact.
///
/// # Parameters
/// * `embedding_model` — stable model id (e.g. `"bge-small-en-v1.5"`).
/// * `embedding_dimension` — vector dimension (e.g. `384`).
/// * `reranker_model` — stable reranker model id (e.g. `"bge-reranker-base"`).
/// * `chunker_version` — `ohara_parse::CHUNKER_VERSION`.
/// * `parser_versions` — `ohara_parse::parser_versions()`.
pub fn runtime_metadata_from(
    embedding_model: impl Into<String>,
    embedding_dimension: u32,
    reranker_model: impl Into<String>,
    chunker_version: impl Into<String>,
    parser_versions: BTreeMap<String, String>,
) -> RuntimeIndexMetadata {
    RuntimeIndexMetadata {
        schema_version: SCHEMA_VERSION.to_string(),
        embedding_model: embedding_model.into(),
        embedding_dimension,
        reranker_model: reranker_model.into(),
        chunker_version: chunker_version.into(),
        semantic_text_version: SEMANTIC_TEXT_VERSION.to_string(),
        parser_versions,
    }
}

/// Compose a single hint string from the freshness state and the
/// compatibility verdict.
///
/// Returns `None` when the index is fresh and fully compatible.
/// Returns `Some(hint)` describing what is wrong and what command fixes it.
/// When both freshness and compatibility issues exist the two hint strings
/// are joined with a space so the caller gets a single actionable message.
///
/// This function is the single canonical implementation; `ohara-engine` and
/// `ohara-mcp` both call it via `ohara_core::index_metadata::compose_hint`.
pub fn compose_hint(st: &IndexStatus, compatibility: &CompatibilityStatus) -> Option<String> {
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

    fn runtime_baseline() -> RuntimeIndexMetadata {
        let mut parsers = BTreeMap::new();
        parsers.insert("rust".to_string(), "1".to_string());
        parsers.insert("python".to_string(), "1".to_string());
        RuntimeIndexMetadata {
            schema_version: "3".to_string(),
            embedding_model: "BAAI/bge-small-en-v1.5".to_string(),
            embedding_dimension: 384,
            reranker_model: "bge-reranker-base".to_string(),
            chunker_version: "1".to_string(),
            semantic_text_version: "1".to_string(),
            parser_versions: parsers,
        }
    }

    fn stored_complete(runtime: &RuntimeIndexMetadata) -> StoredIndexMetadata {
        let mut components = BTreeMap::new();
        components.insert("schema".into(), runtime.schema_version.clone());
        components.insert("embedding_model".into(), runtime.embedding_model.clone());
        components.insert(
            "embedding_dimension".into(),
            runtime.embedding_dimension.to_string(),
        );
        components.insert("reranker_model".into(), runtime.reranker_model.clone());
        components.insert("chunker_version".into(), runtime.chunker_version.clone());
        components.insert(
            "semantic_text_version".into(),
            runtime.semantic_text_version.clone(),
        );
        for (lang, ver) in &runtime.parser_versions {
            components.insert(format!("parser_{lang}"), ver.clone());
        }
        StoredIndexMetadata { components }
    }

    #[test]
    fn exact_match_is_compatible() {
        let runtime = runtime_baseline();
        let stored = stored_complete(&runtime);
        assert_eq!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::Compatible
        );
    }

    #[test]
    fn missing_metadata_is_unknown() {
        let runtime = runtime_baseline();
        let stored = StoredIndexMetadata::default();
        match CompatibilityStatus::assess(&runtime, &stored) {
            CompatibilityStatus::Unknown { missing_components } => {
                assert!(missing_components.contains(&"embedding_model".to_string()));
                assert!(missing_components.contains(&"chunker_version".to_string()));
                assert!(missing_components.contains(&"parser_rust".to_string()));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn dimension_mismatch_needs_rebuild() {
        let runtime = runtime_baseline();
        let mut stored = stored_complete(&runtime);
        stored
            .components
            .insert("embedding_dimension".into(), "768".into());
        match CompatibilityStatus::assess(&runtime, &stored) {
            CompatibilityStatus::NeedsRebuild { reason } => {
                assert!(
                    reason.contains("embedding_dimension"),
                    "reason should name the offending component: {reason}"
                );
            }
            other => panic!("expected NeedsRebuild, got {other:?}"),
        }
    }

    #[test]
    fn embedding_model_mismatch_needs_rebuild() {
        let runtime = runtime_baseline();
        let mut stored = stored_complete(&runtime);
        stored
            .components
            .insert("embedding_model".into(), "voyage-code-3".into());
        assert!(matches!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::NeedsRebuild { .. }
        ));
    }

    #[test]
    fn chunker_version_mismatch_needs_refresh() {
        let runtime = runtime_baseline();
        let mut stored = stored_complete(&runtime);
        stored
            .components
            .insert("chunker_version".into(), "0".into());
        match CompatibilityStatus::assess(&runtime, &stored) {
            CompatibilityStatus::QueryCompatibleNeedsRefresh { reason } => {
                assert!(reason.contains("chunker_version"));
            }
            other => panic!("expected QueryCompatibleNeedsRefresh, got {other:?}"),
        }
    }

    #[test]
    fn parser_version_mismatch_needs_refresh() {
        let runtime = runtime_baseline();
        let mut stored = stored_complete(&runtime);
        stored.components.insert("parser_rust".into(), "0".into());
        assert!(matches!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::QueryCompatibleNeedsRefresh { .. }
        ));
    }

    #[test]
    fn rebuild_diagnosis_dominates_refresh_diagnosis() {
        // If both a vector-affecting and a derived component differ,
        // the rebuild verdict wins so callers don't try to "just
        // refresh" their way out of a wrong-vector index.
        let runtime = runtime_baseline();
        let mut stored = stored_complete(&runtime);
        stored
            .components
            .insert("embedding_dimension".into(), "768".into());
        stored
            .components
            .insert("chunker_version".into(), "0".into());
        assert!(matches!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::NeedsRebuild { .. }
        ));
    }

    #[test]
    fn status_serialises_with_kind_tag() {
        let s = CompatibilityStatus::Compatible;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"status\":\"compatible\""), "got {json}");

        let s = CompatibilityStatus::NeedsRebuild { reason: "x".into() };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"status\":\"needs_rebuild\""), "got {json}");
    }

    // ---- compose_hint tests (moved from ohara-mcp::server::tests) -----------

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

    // ---- runtime_metadata_from tests ----------------------------------------

    struct StubEmbedder;
    impl crate::EmbeddingProvider for StubEmbedder {
        fn dimension(&self) -> usize {
            384
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
        }
    }

    #[test]
    fn runtime_metadata_from_populates_all_fields() {
        let mut parsers = BTreeMap::new();
        parsers.insert("rust".to_string(), "1".to_string());
        let embedder = StubEmbedder;
        let meta = runtime_metadata_from(&embedder, "bge-reranker-base", "2", parsers.clone());
        assert_eq!(meta.embedding_model, "stub-model");
        assert_eq!(meta.embedding_dimension, 384);
        assert_eq!(meta.reranker_model, "bge-reranker-base");
        assert_eq!(meta.chunker_version, "2");
        assert_eq!(meta.parser_versions, parsers);
        assert_eq!(meta.schema_version, SCHEMA_VERSION);
    }
}
