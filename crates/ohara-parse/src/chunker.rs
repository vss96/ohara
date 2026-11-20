//! AST-aware sibling-merge chunker.
//!
//! Walks a list of source-order top-level symbol "atoms" left-to-right,
//! greedily merging consecutive atoms into a single chunk while the
//! merged chunk's `source_text` (atoms *and* the gaps between them)
//! stays under `max_tokens`. An atom that already
//! exceeds the budget on its own is emitted as a single-symbol chunk
//! (no subdivision).
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

fn tokens_for_chars(chars: usize) -> usize {
    chars / CHARS_PER_TOKEN
}

/// Merge `atoms` (already in source byte order) into AST-aware chunks
/// up to `max_tokens` per chunk.
///
/// Algorithm (depth-first source-byte-order traversal):
/// 1. Walk atoms left-to-right. Maintain a "current chunk" of
///    `(primary, siblings, running_chars)`, where `running_chars` is
///    the char count of the chunk's eventual `source_text` — the
///    slice of `source` from the primary's start to the last
///    sibling's end, *including* any gap bytes (comments, docstrings,
///    whitespace, uncaptured declarations) between atoms.
/// 2. For each next atom A, let `extra` be the chars from the current
///    chunk's end up to A's end (gap + A):
///    - If `estimate(running_chars + extra) <= max_tokens`, append A
///      to the current chunk's siblings and extend the span.
///    - Otherwise close the current chunk and start a new one with A
///      as primary.
///    - If A alone exceeds `max_tokens`, emit it as its own
///      single-atom chunk (no subdivision).
/// 3. Flush any non-empty chunk at end of input.
///
/// Counting the gap keeps every merged chunk's `source_text` within
/// `max_tokens` (#121); single-atom chunks may still exceed it.
pub fn chunk_symbols(atoms: Vec<Symbol>, max_tokens: usize, source: &str) -> Vec<Symbol> {
    let mut out: Vec<Symbol> = Vec::new();
    let mut current: Option<ChunkBuilder> = None;

    for atom in atoms {
        let Some(mut buf) = current.take() else {
            current = Some(ChunkBuilder::start(atom, source));
            continue;
        };
        let extra = buf.extra_chars(&atom, source);
        if tokens_for_chars(buf.running_chars + extra) <= max_tokens {
            buf.append(atom, extra);
            current = Some(buf);
            continue;
        }
        out.push(buf.finish(source));
        current = Some(ChunkBuilder::start(atom, source));
    }

    if let Some(buf) = current {
        out.push(buf.finish(source));
    }
    out
}

/// Char count of `source[start..end]`, or of `fallback` when the span
/// doesn't slice `source` cleanly (mismatched source / overlapping or
/// out-of-order atoms).
fn span_chars(source: &str, start: u32, end: u32, fallback: &str) -> usize {
    source
        .get(start as usize..end as usize)
        .unwrap_or(fallback)
        .chars()
        .count()
}

/// Accumulator for a single in-progress chunk during the merge walk.
struct ChunkBuilder {
    primary: Symbol,
    sibling_names: Vec<String>,
    span_end: u32,
    running_chars: usize,
}

impl ChunkBuilder {
    fn start(primary: Symbol, source: &str) -> Self {
        let span_end = primary.span_end;
        let running_chars = span_chars(
            source,
            primary.span_start,
            primary.span_end,
            &primary.source_text,
        );
        Self {
            primary,
            sibling_names: Vec::new(),
            span_end,
            running_chars,
        }
    }

    /// Chars `sibling` would add to this chunk's `source_text`: the
    /// gap after the current end plus the sibling itself.
    fn extra_chars(&self, sibling: &Symbol, source: &str) -> usize {
        span_chars(
            source,
            self.span_end,
            sibling.span_end,
            &sibling.source_text,
        )
    }

    fn append(&mut self, sibling: Symbol, extra_chars: usize) {
        self.sibling_names.push(sibling.name);
        self.span_end = sibling.span_end;
        self.running_chars += extra_chars;
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

    fn estimate_tokens(text: &str) -> usize {
        tokens_for_chars(text.chars().count())
    }
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

    #[test]
    fn chunker_counts_inter_atom_gap_against_budget() {
        // Regression for #121: atoms of 10 + 49 tokens fit a 59-token
        // budget on their own, but a 121-token comment sits between
        // them. The merged slice would be 180 tokens, so they must
        // not merge.
        let (mut atoms, _) = make_atoms(&[10, 49]);
        let gap = "/".repeat(121 * CHARS_PER_TOKEN);
        let first = atoms[0].source_text.clone();
        let second = atoms[1].source_text.clone();
        let source = format!("{first}{gap}{second}");
        atoms[1].span_start = (first.len() + gap.len()) as u32;
        atoms[1].span_end = source.len() as u32;
        let chunks = chunk_symbols(atoms, 59, &source);
        assert_eq!(chunks.len(), 2, "gap must push the merge over budget");
        assert_eq!(chunks[0].source_text, first);
        assert_eq!(chunks[1].source_text, second);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn chunk_symbols_conserves_atoms_order_and_token_budget(
                sizes in proptest::collection::vec(1usize..=400, 0..=25),
                max_tokens in 1usize..=500,
            ) {
                let (atoms, source) = make_atoms(&sizes);
                let expected_names: Vec<String> = atoms.iter().map(|a| a.name.clone()).collect();
                let chunks = chunk_symbols(atoms, max_tokens, &source);

                let mut recovered_names = Vec::new();
                for chunk in &chunks {
                    recovered_names.push(chunk.name.clone());
                    recovered_names.extend(chunk.sibling_names.iter().cloned());
                    prop_assert!(chunk.span_start <= chunk.span_end);
                    if !chunk.sibling_names.is_empty() {
                        prop_assert!(
                            estimate_tokens(&chunk.source_text) <= max_tokens,
                            "multi-atom chunk exceeded max_tokens: {} > {}",
                            estimate_tokens(&chunk.source_text),
                            max_tokens
                        );
                    }
                }
                prop_assert_eq!(recovered_names, expected_names);
            }

            #[test]
            fn chunk_symbols_respects_max_tokens_with_inter_atom_comments(
                sizes in proptest::collection::vec(10usize..=100, 2..=8),
                gap_tokens in 20usize..=150,
                max_tokens in 50usize..=250,
            ) {
                let mut source = String::new();
                let mut atoms = Vec::new();
                let gap = "/".repeat(gap_tokens * CHARS_PER_TOKEN);
                for (i, &tok) in sizes.iter().enumerate() {
                    if i > 0 {
                        source.push_str(&gap);
                    }
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
                let chunks = chunk_symbols(atoms, max_tokens, &source);
                for chunk in &chunks {
                    if !chunk.sibling_names.is_empty() {
                        let actual_tokens = estimate_tokens(&chunk.source_text);
                        prop_assert!(
                            actual_tokens <= max_tokens,
                            "merged chunk source_text has {actual_tokens} tokens > max_tokens {max_tokens}"
                        );
                    }
                }
            }
        }
    }
}
