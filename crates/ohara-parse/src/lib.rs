//! tree-sitter symbol extraction for supported languages.

pub mod chunker;
pub mod java;
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
            chunks.iter().map(|s| s.language.as_str()).collect::<Vec<_>>()
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
