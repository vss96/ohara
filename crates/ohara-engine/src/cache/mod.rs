pub mod blame;
mod embedding;
pub mod meta;

pub use blame::BlameCache;
pub use embedding::EmbeddingCache;
pub use meta::MetaCache;

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
