use ohara_core::types::RepoId;
use ohara_core::{Retriever, Storage};
use ohara_git::Blamer;
use std::path::PathBuf;
use std::sync::Arc;

pub struct RepoHandle {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,
    pub storage: Arc<dyn Storage>,
    pub retriever: Retriever,
    pub blamer: Arc<Blamer>,
}
