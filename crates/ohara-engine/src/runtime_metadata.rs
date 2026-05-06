//! Single canonical builder for [`RuntimeIndexMetadata`] used by every
//! binary that needs to compare the live runtime against an on-disk
//! index (status, MCP find_pattern, engine `compose_response_meta`).
//!
//! Issue #40: callers previously hand-rolled this helper in three
//! places and hardcoded the `embed_input_mode` literal `"semantic"`,
//! which mis-reported `--embed-cache=diff` indexes as
//! `compatibility: needs rebuild`. The single helper here takes an
//! [`EmbedMode`] and routes through the canonical
//! [`EmbedMode::index_metadata_value`] so the literal lives in
//! exactly one place.

use ohara_core::index_metadata::{runtime_metadata_from, RuntimeIndexMetadata};
use ohara_core::EmbedMode;

/// Build the [`RuntimeIndexMetadata`] expected by the current binary
/// for compatibility assessment, using the supplied [`EmbedMode`] as
/// the source of truth for `embed_input_mode`.
///
/// Sources every other field from the static `ohara-embed` /
/// `ohara-parse` constants — does **not** load the embedder model.
/// Callers that don't statically know the on-disk mode (status, MCP
/// query path) typically pass [`EmbedMode::default`] and then override
/// `embed_input_mode` from the stored metadata to avoid false-positive
/// `NeedsRebuild` verdicts on internally-consistent indexes.
pub fn current_runtime_metadata(mode: EmbedMode) -> RuntimeIndexMetadata {
    runtime_metadata_from(
        ohara_embed::DEFAULT_MODEL_ID,
        ohara_embed::DEFAULT_DIM as u32,
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
        mode.index_metadata_value(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_routes_embed_mode_through_index_metadata_value() {
        // Issue #40: helper MUST NOT hardcode the mode literal — it
        // must call `EmbedMode::index_metadata_value()` so Diff mode
        // surfaces as "diff" rather than the previous "semantic".
        assert_eq!(
            current_runtime_metadata(EmbedMode::Off).embed_input_mode,
            "semantic"
        );
        assert_eq!(
            current_runtime_metadata(EmbedMode::Semantic).embed_input_mode,
            "semantic"
        );
        assert_eq!(
            current_runtime_metadata(EmbedMode::Diff).embed_input_mode,
            "diff"
        );
    }

    #[test]
    fn helper_populates_constants_without_loading_embedder() {
        // Mirrors the no-embedder-load contract pinned by the
        // status-side test: every field comes from a static constant.
        let m = current_runtime_metadata(EmbedMode::Semantic);
        assert_eq!(m.embedding_model, ohara_embed::DEFAULT_MODEL_ID);
        assert_eq!(m.embedding_dimension, ohara_embed::DEFAULT_DIM as u32);
        assert_eq!(m.reranker_model, ohara_embed::DEFAULT_RERANKER_ID);
        assert_eq!(m.chunker_version, ohara_parse::CHUNKER_VERSION);
        for lang in ["rust", "python", "java", "kotlin"] {
            assert!(
                m.parser_versions.contains_key(lang),
                "parser_versions missing language {lang}"
            );
        }
    }
}
