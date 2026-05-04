use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use ohara_core::explain::BlameRange;
use ohara_core::types::RepoId;

#[derive(PartialEq, Eq, Hash, Clone)]
struct Key {
    repo_id: RepoId,
    file: String,
    content_hash: String,
}

/// LRU cache keyed by `(RepoId, file, content_hash)` → shared blame result.
///
/// Thread-safe: wraps the inner [`LruCache`] in a [`Mutex`] so it can be
/// shared across async tasks via `Arc<BlameCache>`.
///
/// `content_hash` is the HEAD blob OID (40-char hex) for the file, which
/// changes whenever the file's content changes, providing natural invalidation.
/// `invalidate_repo` drops every entry whose `RepoId` matches the argument.
pub struct BlameCache {
    inner: Mutex<LruCache<Key, Arc<Vec<BlameRange>>>>,
}

impl BlameCache {
    /// Create a new cache with the given LRU `capacity`.
    ///
    /// If `capacity` is 0 it is promoted to 1 so the `NonZeroUsize`
    /// constructor cannot fail. The `expect` below documents that invariant
    /// rather than recovering from an actual runtime condition.
    #[allow(clippy::expect_used)]
    // INVARIANT: capacity.max(1) >= 1, so NonZeroUsize::new always succeeds here.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity.max(1)).expect("capacity is non-zero"),
            )),
        }
    }

    /// Look up `(repo_id, file, content_hash)` in the cache; returns `None`
    /// on a miss or poisoned lock.
    pub fn get(
        &self,
        repo_id: &RepoId,
        file: &str,
        content_hash: &str,
    ) -> Option<Arc<Vec<BlameRange>>> {
        let mut g = self.inner.lock().ok()?;
        g.get(&Key {
            repo_id: repo_id.clone(),
            file: file.into(),
            content_hash: content_hash.into(),
        })
        .cloned()
    }

    /// Insert `value` for the given key, evicting the LRU entry if at capacity.
    /// Silently no-ops on a poisoned lock so the caller re-blames on next access.
    pub fn put(
        &self,
        repo_id: RepoId,
        file: String,
        content_hash: String,
        value: Arc<Vec<BlameRange>>,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            g.put(
                Key {
                    repo_id,
                    file,
                    content_hash,
                },
                value,
            );
        }
    }

    /// Remove all entries whose `repo_id` matches `repo_id`.
    ///
    /// Called from `RetrievalEngine::invalidate_repo` so blame results
    /// computed against stale HEAD content are evicted on index refresh.
    pub fn invalidate_repo(&self, repo_id: &RepoId) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        let keys: Vec<Key> = g
            .iter()
            .filter(|(k, _)| &k.repo_id == repo_id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            g.pop(&k);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ohara_core::types::RepoId;

    fn rid(s: &str) -> RepoId {
        RepoId::from_parts(s, "/x")
    }

    #[test]
    fn put_then_get_returns_arc_eq() {
        let c = BlameCache::new(8);
        let v = std::sync::Arc::new(vec![]);
        c.put(rid("a"), "f.rs".into(), "abc".into(), v.clone());
        let got = c.get(&rid("a"), "f.rs", "abc").expect("hit");
        assert!(std::sync::Arc::ptr_eq(&got, &v));
    }

    #[test]
    fn miss_after_capacity_eviction() {
        let c = BlameCache::new(2);
        c.put(
            rid("a"),
            "f.rs".into(),
            "h1".into(),
            std::sync::Arc::new(vec![]),
        );
        c.put(
            rid("a"),
            "f.rs".into(),
            "h2".into(),
            std::sync::Arc::new(vec![]),
        );
        c.put(
            rid("a"),
            "f.rs".into(),
            "h3".into(),
            std::sync::Arc::new(vec![]),
        ); // evicts h1
        assert!(c.get(&rid("a"), "f.rs", "h1").is_none());
        assert!(c.get(&rid("a"), "f.rs", "h2").is_some());
        assert!(c.get(&rid("a"), "f.rs", "h3").is_some());
    }

    #[test]
    fn different_files_do_not_collide() {
        let c = BlameCache::new(8);
        c.put(
            rid("a"),
            "x.rs".into(),
            "h".into(),
            std::sync::Arc::new(vec![]),
        );
        assert!(c.get(&rid("a"), "y.rs", "h").is_none());
    }

    #[test]
    fn invalidate_repo_removes_only_matching_entries() {
        let c = BlameCache::new(8);
        c.put(
            rid("a"),
            "f.rs".into(),
            "h".into(),
            std::sync::Arc::new(vec![]),
        );
        c.put(
            rid("b"),
            "f.rs".into(),
            "h".into(),
            std::sync::Arc::new(vec![]),
        );
        c.invalidate_repo(&rid("a"));
        assert!(c.get(&rid("a"), "f.rs", "h").is_none());
        assert!(c.get(&rid("b"), "f.rs", "h").is_some());
    }
}
