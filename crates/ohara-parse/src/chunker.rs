//! AST-aware sibling-merge chunker.
//!
//! Track C / step C-2 of plan 3. Walks a list of source-order top-level
//! symbol "atoms" left-to-right, greedily merging consecutive atoms into
//! a single chunk while the running token total stays under
//! `max_tokens`. An atom that already exceeds the budget on its own is
//! emitted as a single-symbol chunk (no subdivision).
//!
//! The emitted `Symbol`'s `name` is the first atom's name (the
//! "primary"); `sibling_names` carries the *other* atoms' names in
//! source order. `kind` and `language` come from the primary; the span
//! covers `[primary.span_start, last_sibling.span_end)`; `source_text`
//! is the slice of `source` over that span (which preserves whitespace
//! between atoms).

use ohara_core::types::Symbol;

/// Approximate tokens-from-chars ratio used for chunking decisions.
/// Four chars per token is a coarse but stable heuristic; the chunker
/// uses it only to decide whether to merge — never to compute embed
/// costs or downstream model budgets — so the approximation is fine.
const CHARS_PER_TOKEN: usize = 4;

fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / CHARS_PER_TOKEN
}

/// Merge `atoms` (already in source byte order) into AST-aware chunks
/// up to `max_tokens` per chunk.
///
/// Algorithm (depth-first source-byte-order traversal):
/// 1. Walk atoms left-to-right. Maintain a "current chunk" of
///    `(primary, siblings, running_tokens)`.
/// 2. For each next atom A:
///    - If `running_tokens + A.tokens <= max_tokens`, append A to the
///      current chunk's siblings and extend the span.
///    - Otherwise close the current chunk (if non-empty) and start a
///      new one with A as primary.
///    - If A alone exceeds `max_tokens`, emit it as its own
///      single-atom chunk (no subdivision).
/// 3. Flush any non-empty chunk at end of input.
pub fn chunk_symbols(atoms: Vec<Symbol>, max_tokens: usize, source: &str) -> Vec<Symbol> {
    let mut out: Vec<Symbol> = Vec::new();
    let mut current: Option<ChunkBuilder> = None;

    for atom in atoms {
        let atom_tokens = estimate_tokens(&atom.source_text);

        match current.take() {
            None => {
                current = Some(ChunkBuilder::start(atom, atom_tokens));
            }
            Some(mut buf) => {
                if buf.running_tokens + atom_tokens <= max_tokens {
                    buf.append(atom, atom_tokens);
                    current = Some(buf);
                } else {
                    out.push(buf.finish(source));
                    current = Some(ChunkBuilder::start(atom, atom_tokens));
                }
            }
        }
    }

    if let Some(buf) = current {
        out.push(buf.finish(source));
    }
    out
}

/// Accumulator for a single in-progress chunk during the merge walk.
struct ChunkBuilder {
    primary: Symbol,
    sibling_names: Vec<String>,
    span_end: u32,
    running_tokens: usize,
}

impl ChunkBuilder {
    fn start(primary: Symbol, tokens: usize) -> Self {
        let span_end = primary.span_end;
        Self {
            primary,
            sibling_names: Vec::new(),
            span_end,
            running_tokens: tokens,
        }
    }

    fn append(&mut self, sibling: Symbol, tokens: usize) {
        self.sibling_names.push(sibling.name);
        self.span_end = sibling.span_end;
        self.running_tokens += tokens;
    }

    fn finish(self, source: &str) -> Symbol {
        let start = self.primary.span_start as usize;
        let end = self.span_end as usize;
        let source_text = source
            .get(start..end)
            .map(str::to_string)
            // Defensive: if span lies outside `source` (e.g. caller
            // passed mismatched source) fall back to the primary's
            // own source_text so we still emit a usable chunk.
            .unwrap_or(self.primary.source_text);
        Symbol {
            file_path: self.primary.file_path,
            language: self.primary.language,
            kind: self.primary.kind,
            name: self.primary.name,
            qualified_name: self.primary.qualified_name,
            sibling_names: self.sibling_names,
            span_start: self.primary.span_start,
            span_end: self.span_end,
            blob_sha: self.primary.blob_sha,
            source_text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::types::{Symbol, SymbolKind};

    /// Build a synthetic atom with `tok` tokens worth of source. Each
    /// atom's source text is `'x'.repeat(tok * CHARS_PER_TOKEN)`, so
    /// `estimate_tokens(source) == tok` exactly. Atoms are laid out
    /// contiguously in `source` so the chunker can slice by span.
    fn make_atoms(sizes: &[usize]) -> (Vec<Symbol>, String) {
        let mut source = String::new();
        let mut atoms = Vec::new();
        for (i, &tok) in sizes.iter().enumerate() {
            let start = source.len();
            let body = "x".repeat(tok * CHARS_PER_TOKEN);
            source.push_str(&body);
            let end = source.len();
            atoms.push(Symbol {
                file_path: "a.rs".into(),
                language: "rust".into(),
                kind: SymbolKind::Function,
                name: format!("fn_{i}"),
                qualified_name: None,
                sibling_names: Vec::new(),
                span_start: start as u32,
                span_end: end as u32,
                blob_sha: "sha".into(),
                source_text: body,
            });
        }
        (atoms, source)
    }

    #[test]
    fn chunker_emits_three_chunks_for_50_600_200_fixture() {
        // Source order [50, 600, 200]; budget 500.
        // - fn_0 (50): start chunk.
        // - fn_1 (600): adding would push to 650 > 500; close fn_0
        //   alone; start new chunk with fn_1; fn_1 alone exceeds budget,
        //   emit immediately.
        // - fn_2 (200): start fresh; flush at EOF.
        // Expected: 3 chunks, each with sibling_names == [].
        let (atoms, source) = make_atoms(&[50, 600, 200]);
        let chunks = chunk_symbols(atoms, 500, &source);
        assert_eq!(chunks.len(), 3, "expected three single-atom chunks");
        let names: Vec<&str> = chunks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["fn_0", "fn_1", "fn_2"]);
        for c in &chunks {
            assert!(
                c.sibling_names.is_empty(),
                "single-atom chunk {} should have empty sibling_names",
                c.name
            );
        }
    }

    #[test]
    fn chunker_merges_consecutive_small_atoms_into_one_chunk() {
        // Three 100-token atoms = 300 total, well under 500. All merge.
        let (atoms, source) = make_atoms(&[100, 100, 100]);
        let chunks = chunk_symbols(atoms, 500, &source);
        assert_eq!(chunks.len(), 1, "expected a single merged chunk");
        let c = &chunks[0];
        assert_eq!(c.name, "fn_0", "primary should be the first atom");
        assert_eq!(
            c.sibling_names,
            vec!["fn_1".to_string(), "fn_2".to_string()],
            "siblings should list the merged-in atoms in source order"
        );
        // Span should cover all three atoms.
        assert_eq!(c.span_start, 0);
        assert_eq!(c.span_end as usize, source.len());
    }

    #[test]
    fn chunker_emits_oversized_atom_alone() {
        // One 800-token atom; budget 500. Atom exceeds budget on its
        // own, so we emit it as a single-symbol chunk (no subdivision).
        let (atoms, source) = make_atoms(&[800]);
        let chunks = chunk_symbols(atoms, 500, &source);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].name, "fn_0");
        assert!(chunks[0].sibling_names.is_empty());
    }

    #[test]
    fn chunker_preserves_source_byte_order_in_sibling_names() {
        // Four 50-token atoms = 200 total; all merge. sibling_names
        // should list fn_1, fn_2, fn_3 in source order (not arbitrary).
        let (atoms, source) = make_atoms(&[50, 50, 50, 50]);
        let chunks = chunk_symbols(atoms, 500, &source);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].sibling_names,
            vec!["fn_1".to_string(), "fn_2".to_string(), "fn_3".to_string()]
        );
    }
}
