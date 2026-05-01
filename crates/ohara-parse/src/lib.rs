//! tree-sitter symbol extraction for supported languages.

pub mod chunker;
pub mod java;
pub mod kotlin;
pub mod python;
pub mod rust;

use anyhow::Result;
use ohara_core::indexer::SymbolSource;
use ohara_core::types::Symbol;
use std::path::{Path, PathBuf};

/// 500-token target budget for the AST sibling-merge chunker. Matches
/// plan 3 §C-2; tuned to stay well under common embedder context
/// limits (e.g. BGE small/base sit at 512).
const CHUNK_MAX_TOKENS: usize = 500;

pub fn extract_for_path(path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    let mut atoms = match ext {
        Some("rs") => rust::extract(path, source, blob_sha)?,
        Some("py") => python::extract(path, source, blob_sha)?,
        Some("java") => java::extract(path, source, blob_sha)?,
        Some("kt") | Some("kts") => kotlin::extract(path, source, blob_sha)?,
        _ => return Ok(vec![]),
    };
    // The chunker requires source-byte-order traversal. Tree-sitter
    // captures in `rust::extract` are emitted in match order which is
    // already source-aligned; `python::extract` dedups through a
    // HashMap whose iteration order is undefined, so we sort here
    // unconditionally — cheap and language-agnostic.
    atoms.sort_by_key(|s| s.span_start);
    Ok(chunker::chunk_symbols(atoms, CHUNK_MAX_TOKENS, source))
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
