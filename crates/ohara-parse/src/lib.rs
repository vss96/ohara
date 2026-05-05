//! tree-sitter chunker and symbol extraction for the languages ohara
//! indexes: Rust, TypeScript, JavaScript, Python, Java, Kotlin.
//!
//! Two responsibilities:
//! - [`chunker`]: AST sibling-merge into <=`CHUNK_MAX_TOKENS` chunks for
//!   embedding. Annotated definitions (e.g. Java methods with their
//!   annotations, Rust fns with their attribute macros) stay whole so a
//!   query for the annotation surfaces the right hunk.
//! - [`languages`]: per-language tree-sitter grammars and the symbol
//!   walkers that extract `Symbol`s for the FTS5 symbol-name lane.
//!
//! [`TreeSitterAtomicExtractor`] is the entry point ohara-core's
//! indexer wires through `Indexer::with_atomic_symbol_extractor`.
//! Parse failures swallow to an empty `Vec<Symbol>` — one unparseable
//! file must not abort the whole index pass.

pub mod chunker;
pub mod languages;

use anyhow::Result;
use ohara_core::indexer::{AtomicSymbolExtractor, SymbolSource};
use ohara_core::types::Symbol;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Plan 11: implements `ohara_core::AtomicSymbolExtractor` by
/// delegating to `extract_atomic_symbols` against the language
/// matching `path`'s extension. Construct via `Default` and pass to
/// `Indexer::with_atomic_symbol_extractor`.
#[derive(Default)]
pub struct TreeSitterAtomicExtractor;

impl AtomicSymbolExtractor for TreeSitterAtomicExtractor {
    fn extract(&self, path: &str, source: &str) -> Vec<Symbol> {
        // Tree-sitter parse failures fall back to an empty Vec —
        // the attributor's HunkHeader path takes over for files that
        // don't parse. We intentionally swallow the error here
        // because the whole indexing pass shouldn't fail just because
        // one file's syntax tree isn't recoverable.
        extract_atomic_symbols(path, source, "").unwrap_or_default()
    }
}

/// 500-token target budget for the AST sibling-merge chunker. Matches
/// plan 3 §C-2; tuned to stay well under common embedder context
/// limits (e.g. BGE small/base sit at 512).
const CHUNK_MAX_TOKENS: usize = 500;

/// AST sibling-merge chunker version (plan 13). Bump this when the
/// chunker's output semantics change in a way that would invalidate
/// previously-indexed `hunk` / `symbol` rows. Stored under the
/// `chunker_version` component key.
///
/// v2 (plan 11): a new atomic-symbol entry-point lands alongside the
/// chunker (`extract_atomic_symbols`). The chunker output itself
/// hasn't changed, but indexes built before v2 don't have the
/// `hunk_symbol` rows that v0.7 retrieval expects — so we bump to
/// trigger a `query-compatible, refresh recommended` verdict for
/// pre-plan-11 indexes.
pub const CHUNKER_VERSION: &str = "2";

/// Returns `language -> parser_version` for every language this crate
/// can index. Used by the indexer to record per-parser metadata so the
/// runtime can detect when an old index was built with a different
/// parser version. Bump a value here when a per-language extractor's
/// output semantics change.
pub fn parser_versions() -> BTreeMap<String, String> {
    [
        ("rust", "2"),   // bumped: tree-sitter-rust 0.21 -> 0.24 may emit subtly different AST
        ("python", "2"), // bumped: tree-sitter-python 0.21 -> 0.25
        ("java", "2"),   // bumped: tree-sitter-java 0.21 -> 0.23
        ("kotlin", "2"), // bumped: grammar swapped to tree-sitter-kotlin-ng
        ("javascript", "1"), // plan-17: initial javascript extractor
        ("typescript", "1"), // plan-17: initial typescript extractor
    ]
    .into_iter()
    .map(|(lang, ver)| (lang.to_string(), ver.to_string()))
    .collect()
}

pub fn extract_for_path(path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut atoms = extract_atomic_symbols(path, source, blob_sha)?;
    // The chunker requires source-byte-order traversal. Tree-sitter
    // captures in `languages::rust::extract` are emitted in match order
    // which is already source-aligned; `languages::python::extract` dedups
    // through a HashMap whose iteration order is undefined, so we sort
    // here unconditionally — cheap and language-agnostic.
    atoms.sort_by_key(|s| s.span_start);
    Ok(chunker::chunk_symbols(atoms, CHUNK_MAX_TOKENS, source))
}

/// Plan 11: extract pre-merge atomic symbols for `path` against
/// `source`, source-byte-order sorted. Same per-language extractors
/// as [`extract_for_path`], but the AST sibling-merge chunker is
/// skipped — callers (the per-hunk symbol attributor in
/// `ohara-core::hunk_attribution`) need the unmerged spans so a
/// hunk that touches one method out of N in a class doesn't get
/// attributed to the entire class.
///
/// Returns an empty Vec for unsupported file extensions; matches
/// `extract_for_path`'s behaviour so unsupported files just don't get
/// hunk-symbol attribution rather than erroring out the whole pass.
pub fn extract_atomic_symbols(path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    let mut atoms = match ext {
        Some("rs") => languages::rust::extract(path, source, blob_sha)?,
        Some("py") => languages::python::extract(path, source, blob_sha)?,
        Some("java") => languages::java::extract(path, source, blob_sha)?,
        Some("kt") | Some("kts") => languages::kotlin::extract(path, source, blob_sha)?,
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => {
            languages::javascript::extract(path, source, blob_sha)?
        }
        Some("ts") => languages::typescript::extract(
            path,
            source,
            blob_sha,
            languages::typescript::TsFlavor::Ts,
        )?,
        Some("tsx") => languages::typescript::extract(
            path,
            source,
            blob_sha,
            languages::typescript::TsFlavor::Tsx,
        )?,
        _ => return Ok(vec![]),
    };
    atoms.sort_by_key(|s| s.span_start);
    Ok(atoms)
}

/// Convert a `Symbol`'s byte range (`span_start..span_end`) to a
/// 1-based line range `(start, end_inclusive)`. Used by the per-hunk
/// attributor to intersect symbol spans against a hunk's post-image
/// line ranges. `source` is the file body the spans were computed
/// against.
///
/// The conversion walks the source once per symbol — acceptable on
/// per-commit hot-paths because attributed files are bounded by the
/// hunk count, not the project size.
pub fn symbol_line_span(symbol: &Symbol, source: &str) -> (u32, u32) {
    let bytes = source.as_bytes();
    let start = symbol.span_start as usize;
    let end = (symbol.span_end as usize).min(bytes.len());
    let line_at = |pos: usize| -> u32 {
        // Count newlines before `pos`; +1 for 1-based numbering.
        let counted = bytes[..pos.min(bytes.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        u32::try_from(counted + 1).unwrap_or(u32::MAX)
    };
    let line_start = line_at(start);
    let line_end = if end == 0 {
        line_start
    } else {
        line_at(end - 1)
    };
    (line_start, line_end.max(line_start))
}

/// Walks the working tree at HEAD-equivalent state on disk and extracts symbols
/// from files in supported languages.
pub struct GitSymbolSource {
    repo_path: PathBuf,
}

impl GitSymbolSource {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self {
            repo_path: path.as_ref().to_path_buf(),
        })
    }
}

#[async_trait::async_trait]
impl SymbolSource for GitSymbolSource {
    #[tracing::instrument(skip(self), fields(repo = %self.repo_path.display()))]
    async fn extract_head_symbols(&self) -> ohara_core::Result<Vec<Symbol>> {
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<Symbol>> {
            let repo = git2::Repository::discover(&path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let head = repo.head().map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let tree = head.peel_to_tree().map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let mut out = Vec::new();
            tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    let name = match entry.name() {
                        Some(n) => n,
                        None => return git2::TreeWalkResult::Ok,
                    };
                    let p = format!("{}{}", dir, name);
                    let blob_sha = entry.id().to_string();
                    match repo.find_blob(entry.id()) {
                        Ok(blob) => {
                            // Err on from_utf8 means binary file; expected, ignored.
                            if let Ok(s) = std::str::from_utf8(blob.content()) {
                                match extract_for_path(&p, s, &blob_sha) {
                                    Ok(mut syms) => out.append(&mut syms),
                                    Err(e) => tracing::warn!(path = %p, error = %e, "symbol extraction failed; skipping file"),
                                }
                            }
                        }
                        Err(e) => tracing::warn!(path = %p, error = %e, "blob lookup failed; skipping file"),
                    }
                }
                git2::TreeWalkResult::Ok
            })
            .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            tracing::info!(symbols_extracted = out.len(), "head symbol extraction complete");
            Ok(out)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Other(anyhow::anyhow!(e)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_atomic_symbols_dispatches_javascript_extensions() {
        let src = "function alpha() {}\n";
        for ext in ["js", "jsx", "mjs", "cjs"] {
            let path = format!("a.{ext}");
            let syms = extract_atomic_symbols(&path, src, "deadbeef").expect("dispatch");
            assert!(
                syms.iter().any(|s| s.name == "alpha"),
                "{ext} did not dispatch to javascript extractor: {syms:?}"
            );
        }
    }

    #[test]
    fn extract_atomic_symbols_dispatches_typescript_extensions() {
        let src = "function alpha() {}\n";
        for ext in ["ts", "tsx"] {
            let path = format!("a.{ext}");
            let syms = extract_atomic_symbols(&path, src, "deadbeef").expect("dispatch");
            assert!(
                syms.iter().any(|s| s.name == "alpha"),
                "{ext} did not dispatch to typescript extractor: {syms:?}"
            );
        }
    }

    #[test]
    fn extract_atomic_symbols_returns_pre_merge_atoms() {
        // Plan 11 Task 3.1 Step 1: atomic extractor MUST return one
        // symbol per top-level definition (not the merged super-chunk).
        // Three small Rust functions sit well under the chunker's
        // 500-token budget, so extract_for_path returns 1 merged chunk
        // — extract_atomic_symbols must return 3.
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let atoms = extract_atomic_symbols("a.rs", src, "deadbeef").expect("atoms");
        assert_eq!(
            atoms.len(),
            3,
            "expected three atomic symbols (alpha/beta/gamma), got {atoms:?}"
        );
        let names: Vec<&str> = atoms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn symbol_line_span_translates_byte_offsets_to_1_based_line_numbers() {
        // Plan 11 Task 3.1 Step 1: the line-span helper backs the
        // per-hunk attributor. Three single-line Rust functions; each
        // symbol's byte range covers one source line.
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let atoms = extract_atomic_symbols("a.rs", src, "deadbeef").expect("atoms");
        for (i, atom) in atoms.iter().enumerate() {
            let (start, end) = symbol_line_span(atom, src);
            let expected_line = u32::try_from(i + 1).unwrap();
            assert_eq!(
                (start, end),
                (expected_line, expected_line),
                "symbol {} ('{}') should sit on line {expected_line}",
                i,
                atom.name
            );
        }
    }

    #[test]
    fn extract_for_path_routes_kt_and_kts_to_kotlin_module() {
        // Plan 4 / Task 11: both .kt (production source) and .kts
        // (script) extensions must route through kotlin::extract.
        for path in &["Foo.kt", "build.gradle.kts"] {
            let src = "class Foo { fun bar() {} }\n";
            let chunks = extract_for_path(path, src, "deadbeef").expect("extract");
            assert!(
                !chunks.is_empty(),
                "expected at least one chunk for {path}, got {chunks:?}"
            );
            assert!(
                chunks.iter().all(|s| s.language == "kotlin"),
                "all chunks for {path} should carry language=kotlin"
            );
        }
    }

    #[test]
    fn extract_for_path_routes_java_to_java_module() {
        // Plan 4 / Task 6: a .java path must route through java::extract
        // and produce at least one symbol. We assert via the language
        // tag rather than counting symbols so chunker merges don't
        // change the assertion's meaning.
        let src = "public class Foo { public void run() {} }\n";
        let chunks = extract_for_path("Foo.java", src, "deadbeef").expect("extract");
        assert!(
            !chunks.is_empty(),
            "expected at least one chunk for .java, got {chunks:?}"
        );
        assert!(
            chunks.iter().all(|s| s.language == "java"),
            "all chunks should carry language=java, got {:?}",
            chunks
                .iter()
                .map(|s| s.language.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn spring_fixture_preserves_annotations_through_extract_for_path() {
        // Plan 4 / Task 13: an end-to-end check that a Spring-flavored
        // controller round-trips through the public extract_for_path
        // entry (not just the per-language module) with class- and
        // method-level annotations intact in source_text. This is the
        // signal that downstream embedding + BM25 picks up Spring
        // tokens.
        let src = "\
package com.example.web;

@RestController
@RequestMapping(\"/users\")
public class UserController {
    @GetMapping(\"/{id}\")
    public User get(@PathVariable Long id) { return null; }

    @PostMapping
    public User create(@RequestBody UserRequest req) { return null; }
}
";
        let chunks = extract_for_path("UserController.java", src, "deadbeef").expect("extract");
        // The chunker may merge the class with its small methods into
        // a single chunk. Either way, the merged chunk's source_text
        // must contain every annotation we care about — that's the
        // contract this test guards.
        let combined: String = chunks.iter().map(|c| c.source_text.as_str()).collect();
        for needle in &[
            "@RestController",
            "@RequestMapping(\"/users\")",
            "@GetMapping(\"/{id}\")",
            "@PathVariable",
            "@PostMapping",
            "@RequestBody",
        ] {
            assert!(
                combined.contains(needle),
                "extract_for_path output should contain {needle}, got chunks: {chunks:?}"
            );
        }
    }

    #[test]
    fn chunker_merges_small_java_methods_up_to_500_tokens() {
        // Plan 4 / Task 12: the AST-aware chunker is language-agnostic
        // — it consumes the per-file flat list of source-order atoms
        // emitted by java::extract. A small Java class with a couple
        // of trivial methods sits well under 500 tokens, so the
        // chunker should merge the class + its methods into a single
        // chunk whose primary name is the class and whose
        // sibling_names lists the merged methods (and constructor) in
        // source order.
        let src = "\
public class Calc {
    public Calc() {}
    public int add(int a, int b) { return a + b; }
    public int sub(int a, int b) { return a - b; }
}
";
        let chunks = extract_for_path("Calc.java", src, "deadbeef").expect("extract");
        assert_eq!(chunks.len(), 1, "expected one merged chunk, got {chunks:?}");
        let c = &chunks[0];
        assert_eq!(c.name, "Calc", "primary should be the source-first atom");
        // Constructor + two methods get merged in as siblings (source
        // byte order). The exact order is constructor, add, sub.
        assert_eq!(
            c.sibling_names,
            vec!["Calc".to_string(), "add".to_string(), "sub".to_string()],
            "siblings should appear in source byte order"
        );
    }

    #[test]
    fn chunker_emits_kotlin_data_classes_as_chunks() {
        // Plan 4 / Task 12: same chunker contract for Kotlin. Two
        // tiny `data class` declarations should round-trip cleanly:
        // the chunker merges them when the combined token estimate
        // stays under budget.
        let src = "\
data class A(val x: Int)
data class B(val y: Int)
";
        let chunks = extract_for_path("AB.kt", src, "deadbeef").expect("extract");
        assert!(
            !chunks.is_empty(),
            "expected at least one chunk, got {chunks:?}"
        );
        // Whether the two data classes merge into one chunk or stay
        // separate depends on the 500-token threshold; either is
        // acceptable. We only assert the language tag and that one of
        // the chunks names class A as primary or sibling.
        for c in &chunks {
            assert_eq!(c.language, "kotlin");
        }
        let mentioned: Vec<String> = chunks
            .iter()
            .flat_map(|c| std::iter::once(c.name.clone()).chain(c.sibling_names.iter().cloned()))
            .collect();
        assert!(
            mentioned.contains(&"A".to_string()),
            "expected class A in some chunk, got {mentioned:?}"
        );
        assert!(
            mentioned.contains(&"B".to_string()),
            "expected class B in some chunk, got {mentioned:?}"
        );
    }

    #[test]
    fn extract_for_path_emits_sibling_names_for_merged_chunks() {
        // Three small Rust functions; well under the 500-token budget,
        // so the chunker should merge them into a single chunk whose
        // sibling_names lists the second and third in source order.
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let chunks = extract_for_path("a.rs", src, "deadbeef").expect("extract");
        assert_eq!(chunks.len(), 1, "expected one merged chunk for tiny file");
        let c = &chunks[0];
        assert_eq!(c.name, "alpha", "primary should be first source-order atom");
        assert_eq!(
            c.sibling_names,
            vec!["beta".to_string(), "gamma".to_string()],
            "siblings should appear in source byte order"
        );
    }
}
