//! tree-sitter symbol extraction for supported languages.

pub mod chunker;
pub mod python;
pub mod rust;

use anyhow::Result;
use ohara_core::indexer::SymbolSource;
use ohara_core::types::Symbol;
use std::path::{Path, PathBuf};

pub fn extract_for_path(path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    match ext {
        Some("rs") => rust::extract(path, source, blob_sha),
        Some("py") => python::extract(path, source, blob_sha),
        _ => Ok(vec![]),
    }
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
