use ohara_core::query::ResponseMeta;
use ohara_core::types::RepoId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// TTL-bounded cache for [`ResponseMeta`] keyed by [`RepoId`].
///
/// Thread-safe: wraps the inner map in a [`Mutex`] so it can be shared
/// across async tasks via `Arc<MetaCache>`. Intended to memoize the `_meta`
/// response field for a short window (e.g. 5 s) to avoid hitting storage on
/// every MCP tool call.
pub struct MetaCache {
    ttl: Duration,
    inner: Mutex<HashMap<RepoId, (Instant, ResponseMeta)>>,
    /// Number of `get` calls served from a fresh (non-expired) entry.
    hits: AtomicU64,
}

impl MetaCache {
    /// Create a new cache whose entries expire after `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
        }
    }

    /// Return the cached value for `k` if it exists and is still within the
    /// TTL window. Returns `None` on a miss, an expired entry, or a poisoned
    /// lock.
    pub fn get(&self, k: &RepoId) -> Option<ResponseMeta> {
        let g = self.inner.lock().ok()?;
        let (t, v) = g.get(k)?;
        if t.elapsed() > self.ttl {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(v.clone())
    }

    /// Number of `get` calls served from a fresh entry. Misses and
    /// TTL-expired entries are not counted.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Insert or overwrite the cached value for `k`, stamping it with the
    /// current instant. Silently no-ops on a poisoned lock.
    pub fn put(&self, k: RepoId, v: ResponseMeta) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(k, (Instant::now(), v));
        }
    }

    /// Remove any cached entry for `k`. Silently no-ops on a poisoned lock.
    pub fn invalidate(&self, k: &RepoId) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(k);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ohara_core::query::{IndexStatus, ResponseMeta};

    fn make_meta(hint: &str) -> ResponseMeta {
        ResponseMeta {
            index_status: IndexStatus::default(),
            hint: Some(hint.to_owned()),
            compatibility: None,
        }
    }

    fn make_key() -> RepoId {
        RepoId::from_parts("abc", "/x")
    }

    #[test]
    fn put_get_within_ttl_returns_value() {
        let cache = MetaCache::new(Duration::from_secs(5));
        let key = make_key();
        let meta = make_meta("hello");
        cache.put(key.clone(), meta.clone());
        let got = cache.get(&key).unwrap();
        assert_eq!(got.hint.as_deref(), Some("hello"));
    }

    #[test]
    fn get_after_ttl_returns_none() {
        // Zero-duration TTL: every entry is immediately expired.
        let cache = MetaCache::new(Duration::from_nanos(0));
        let key = make_key();
        cache.put(key.clone(), make_meta("stale"));
        // Spin briefly so elapsed() > 0 ns.
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.get(&key).is_none(), "expired entry must return None");
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = MetaCache::new(Duration::from_secs(60));
        let key = make_key();
        cache.put(key.clone(), make_meta("present"));
        assert!(
            cache.get(&key).is_some(),
            "must be present before invalidate"
        );
        cache.invalidate(&key);
        assert!(cache.get(&key).is_none(), "must be absent after invalidate");
    }

    #[test]
    fn hits_count_only_fresh_cache_hits() {
        let cache = MetaCache::new(Duration::from_secs(60));
        let key = make_key();
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.hits(), 0, "a miss is not a hit");
        cache.put(key.clone(), make_meta("x"));
        cache.get(&key).unwrap();
        cache.get(&key).unwrap();
        assert_eq!(cache.hits(), 2);

        let expired = MetaCache::new(Duration::from_nanos(0));
        expired.put(key.clone(), make_meta("stale"));
        std::thread::sleep(Duration::from_millis(1));
        assert!(expired.get(&key).is_none());
        assert_eq!(expired.hits(), 0, "an expired entry is not a hit");
    }
}
