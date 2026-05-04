//! Output type for the attribute stage.

use super::hunk_chunk::HunkRecord;
use crate::types::Symbol;

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
