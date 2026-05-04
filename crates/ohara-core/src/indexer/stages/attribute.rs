//! Output type and stage implementation for the attribute stage.

use super::hunk_chunk::HunkRecord;
use crate::types::Symbol;

/// A `HunkRecord` extended with optional semantic attribution produced
/// by the attribute stage (tree-sitter atomic-symbol extraction).
#[derive(Debug, Clone)]
pub struct AttributedHunk {
    /// The upstream hunk record.
    pub record: HunkRecord,
    /// Symbols extracted from the post-image source, or `None` when
    /// the source was absent, oversized, or extraction failed.
    pub symbols: Option<Vec<Symbol>>,
    /// Semantic text override produced by attribution (e.g. method
    /// signature prepended to the hunk body). `None` means use
    /// `record.semantic_text` as-is.
    pub attributed_semantic_text: Option<String>,
}

impl AttributedHunk {
    /// Returns the best available semantic text: the attributed
    /// override if present, otherwise the upstream record's text.
    pub fn effective_semantic_text(&self) -> &str {
        self.attributed_semantic_text
            .as_deref()
            .unwrap_or(&self.record.semantic_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::stages::hunk_chunk::HunkRecord;
    use crate::types::Hunk;

    fn make_record(text: &str) -> HunkRecord {
        HunkRecord {
            commit_sha: "abc".into(),
            file_path: "f.rs".into(),
            diff_text: "+x\n".into(),
            semantic_text: text.into(),
            source_hunk: Hunk::default(),
        }
    }

    #[test]
    fn uses_attributed_override_when_present() {
        let h = AttributedHunk {
            record: make_record("base"),
            symbols: None,
            attributed_semantic_text: Some("override".into()),
        };
        assert_eq!(h.effective_semantic_text(), "override");
    }

    #[test]
    fn falls_back_to_record_text_when_no_override() {
        let h = AttributedHunk {
            record: make_record("base"),
            symbols: None,
            attributed_semantic_text: None,
        };
        assert_eq!(h.effective_semantic_text(), "base");
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;
    use crate::indexer::{AtomicSymbolExtractor, MAX_ATTRIBUTABLE_SOURCE_BYTES};
    use crate::indexer::stages::hunk_chunk::HunkRecord;
    use crate::indexer::{CommitSource, SymbolSource};
    use crate::types::{Hunk, CommitMeta, Symbol};
    use crate::Result;
    use async_trait::async_trait;

    fn record(sha: &str, path: &str) -> HunkRecord {
        HunkRecord {
            commit_sha: sha.into(),
            file_path: path.into(),
            diff_text: "+x\n".into(),
            semantic_text: "x".into(),
            source_hunk: Hunk::default(),
        }
    }

    struct NoSymbolSource;
    #[async_trait]
    impl SymbolSource for NoSymbolSource {
        async fn extract_head_symbols(&self) -> Result<Vec<Symbol>> {
            Ok(vec![])
        }
    }

    struct NoAtomicExtractor;
    impl AtomicSymbolExtractor for NoAtomicExtractor {
        fn extract(&self, _path: &str, _source: &str) -> Vec<Symbol> {
            vec![]
        }
    }

    struct PanicAtomicExtractor;
    impl AtomicSymbolExtractor for PanicAtomicExtractor {
        fn extract(&self, _: &str, _: &str) -> Vec<Symbol> {
            panic!("must not be called on oversized source");
        }
    }

    // A CommitSource that returns a source of a configurable size.
    struct SizedSource(usize);
    #[async_trait]
    impl CommitSource for SizedSource {
        async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
            Ok(vec![])
        }
        async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
            Ok(vec![])
        }
        async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(Some("x".repeat(self.0)))
        }
    }

    #[tokio::test]
    async fn hunk_record_without_source_yields_attribution_none() {
        // When file_at_commit returns None (deleted file), attribution
        // must be None and the stage must not error.
        struct AbsentSource;
        #[async_trait]
        impl CommitSource for AbsentSource {
            async fn list_commits(&self, _: Option<&str>) -> Result<Vec<CommitMeta>> {
                Ok(vec![])
            }
            async fn hunks_for_commit(&self, _: &str) -> Result<Vec<Hunk>> {
                Ok(vec![])
            }
            async fn file_at_commit(&self, _: &str, _: &str) -> Result<Option<String>> {
                Ok(None)
            }
        }
        let r = record("abc", "src/deleted.rs");
        let ah = AttributeStage::run(
            &[r],
            "abc",
            &AbsentSource,
            &NoSymbolSource,
            &NoAtomicExtractor,
        )
        .await
        .unwrap();
        assert_eq!(ah.len(), 1);
        assert!(ah[0].symbols.is_none(), "deleted-file hunk must have symbols=None");
    }

    #[tokio::test]
    async fn oversized_source_skips_atomic_extractor() {
        // Source larger than MAX_ATTRIBUTABLE_SOURCE_BYTES must NOT be
        // handed to the extractor (which panics if called).
        let r = record("abc", "vendor/big.min.js");
        let ah = AttributeStage::run(
            &[r],
            "abc",
            &SizedSource(MAX_ATTRIBUTABLE_SOURCE_BYTES + 1),
            &NoSymbolSource,
            &PanicAtomicExtractor,
        )
        .await
        .unwrap();
        assert_eq!(ah.len(), 1);
        assert!(ah[0].symbols.is_none(), "oversized source must yield symbols=None");
    }
}
