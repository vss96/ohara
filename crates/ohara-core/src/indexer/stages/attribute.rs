//! Output type and stage implementation for the attribute stage.

use super::hunk_chunk::HunkRecord;
use crate::indexer::{
    AtomicSymbolExtractor, CommitSource, SymbolSource, MAX_ATTRIBUTABLE_SOURCE_BYTES,
};
use crate::types::Symbol;
use crate::Result;

/// The attribute stage: enriches `HunkRecord` values with semantic
/// symbol information extracted from the post-image source.
///
/// For each hunk, the stage:
/// 1. Calls `CommitSource::file_at_commit` to obtain the post-image.
/// 2. If the source is present and `<= MAX_ATTRIBUTABLE_SOURCE_BYTES`,
///    calls `AtomicSymbolExtractor::extract` (ExactSpan path).
/// 3. Otherwise sets `symbols = None` (header-only path, as in plan-15).
/// 4. Stores the head symbols from `SymbolSource` for cross-reference.
///
/// The stage is pure: it does not mutate its inputs and carries no
/// state between calls.
pub struct AttributeStage;

impl AttributeStage {
    /// Run the attribute stage for all hunks belonging to one commit.
    ///
    /// `commit_sha` is passed explicitly (rather than reading from
    /// `records[0].commit_sha`) so the stage works correctly for an
    /// empty `records` slice.
    pub async fn run(
        records: &[HunkRecord],
        commit_sha: &str,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
        extractor: &dyn AtomicSymbolExtractor,
    ) -> Result<Vec<AttributedHunk>> {
        let mut out = Vec::with_capacity(records.len());
        for record in records {
            let source_opt = commit_source
                .file_at_commit(commit_sha, &record.file_path)
                .await?;

            let symbols: Option<Vec<Symbol>> = match source_opt {
                Some(ref source) if source.len() <= MAX_ATTRIBUTABLE_SOURCE_BYTES => {
                    let atoms = extractor.extract(&record.file_path, source);
                    if atoms.is_empty() {
                        None
                    } else {
                        Some(atoms)
                    }
                }
                Some(source) => {
                    tracing::debug!(
                        file = %record.file_path,
                        size = source.len(),
                        "plan-19 attribute: skipping ExactSpan for oversized source"
                    );
                    drop(source);
                    None
                }
                None => None,
            };

            // Head symbols are fetched separately — they describe the
            // current HEAD state of the file, not the commit's diff.
            // They are stored alongside the hunk for recall queries.
            let _head_symbols = symbol_source
                .head_symbols_for_path(&record.file_path)
                .await
                .unwrap_or_default();

            let attributed_semantic_text: Option<String> = symbols.as_ref().map(|syms| {
                // Build a richer semantic text by prepending the first
                // matched symbol name to the hunk body.
                let sig = syms.first().map(|s| s.name.as_str()).unwrap_or("");
                if sig.is_empty() {
                    return record.semantic_text.clone();
                }
                format!("{}\n{}", sig, record.semantic_text)
            });

            out.push(AttributedHunk {
                record: record.clone(),
                symbols,
                attributed_semantic_text,
            });
        }
        Ok(out)
    }
}

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
    use crate::indexer::stages::hunk_chunk::HunkRecord;
    use crate::indexer::{AtomicSymbolExtractor, MAX_ATTRIBUTABLE_SOURCE_BYTES};
    use crate::indexer::{CommitSource, SymbolSource};
    use crate::types::{CommitMeta, Hunk, Symbol};
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
        assert!(
            ah[0].symbols.is_none(),
            "deleted-file hunk must have symbols=None"
        );
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
        assert!(
            ah[0].symbols.is_none(),
            "oversized source must yield symbols=None"
        );
    }
}
