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

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
        // 1. Vector-affecting components (mismatch -> rebuild).
        let vector_affecting: [(&str, String); 3] = [
            ("embedding_model", runtime.embedding_model.clone()),
            (
                "embedding_dimension",
                runtime.embedding_dimension.to_string(),
            ),
            ("schema", runtime.schema_version.clone()),
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
}
