pub mod blame;
mod embedding;
pub mod meta;

pub use blame::BlameCache;
pub use embedding::EmbeddingCache;
pub use meta::MetaCache;

use ohara_core::types::RepoId;
use std::time::Duration;

/// TTL for cached [`ohara_core::query::ResponseMeta`] entries.
const META_TTL: Duration = Duration::from_secs(5);
/// LRU capacity of the blame cache.
const BLAME_CAPACITY: usize = 64;

/// Every cache whose entries are scoped to a single repo.
///
/// [`Self::invalidate_repo`] destructures `Self` exhaustively, so adding
/// a per-repo cache field without invalidating it is a compile error.
/// [`EmbeddingCache`] deliberately lives outside this group: it is keyed
/// by `(model_id, query text)`, not by repo, so re-indexing a repo
/// cannot make its entries stale.
pub struct RepoCaches {
    pub meta: MetaCache,
    pub blame: BlameCache,
}

impl RepoCaches {
    pub fn new() -> Self {
        Self {
            meta: MetaCache::new(META_TTL),
            blame: BlameCache::new(BLAME_CAPACITY),
        }
    }

    /// Drop every cached entry belonging to `repo_id`.
    pub fn invalidate_repo(&self, repo_id: &RepoId) {
        let Self { meta, blame } = self;
        meta.invalidate(repo_id);
        blame.invalidate_repo(repo_id);
    }
}

impl Default for RepoCaches {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ohara_core::query::{IndexStatus, ResponseMeta};
    use ohara_core::types::RepoId;
    use std::sync::Arc;

    fn meta() -> ResponseMeta {
        ResponseMeta {
            index_status: IndexStatus::default(),
            hint: None,
            compatibility: None,
        }
    }

    #[test]
    fn invalidate_repo_clears_every_per_repo_cache_for_that_repo_only() {
        let caches = RepoCaches::new();
        let a = RepoId::from_parts("a", "/a");
        let b = RepoId::from_parts("b", "/b");
        for id in [&a, &b] {
            caches.meta.put(id.clone(), meta());
            caches
                .blame
                .put(id.clone(), "f.rs".into(), "h".into(), Arc::new(vec![]));
        }

        caches.invalidate_repo(&a);

        assert!(caches.meta.get(&a).is_none());
        assert!(caches.blame.get(&a, "f.rs", "h").is_none());
        assert!(caches.meta.get(&b).is_some(), "other repo untouched");
        assert!(caches.blame.get(&b, "f.rs", "h").is_some());
    }
}
