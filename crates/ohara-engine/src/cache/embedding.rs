use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// LRU cache keyed by `blake3(model_id ":" text)` → shared embedding vector.
///
/// Thread-safe: wraps the inner [`LruCache`] in a [`Mutex`] so it can be
/// shared across async tasks via `Arc<EmbeddingCache>`.
///
/// One cache instance is scoped to a single model id; do not share across models.
pub struct EmbeddingCache {
    model_id: String,
    inner: Mutex<LruCache<[u8; 32], Arc<Vec<f32>>>>,
}

impl EmbeddingCache {
    /// Create a new cache for `model_id` with the given LRU `capacity`.
    ///
    /// If `capacity` is 0 it is promoted to 1 so the `NonZeroUsize`
    /// constructor cannot fail.  The `expect` below documents that invariant
    /// rather than recovering from an actual runtime condition.
    #[allow(clippy::expect_used)]
    // INVARIANT: capacity.max(1) >= 1, so NonZeroUsize::new always succeeds here.
    pub fn new(model_id: impl Into<String>, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity is non-zero");
        Self {
            model_id: model_id.into(),
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    fn cache_key(&self, text: &str) -> [u8; 32] {
        let input = format!("{}:{}", self.model_id, text);
        *blake3::hash(input.as_bytes()).as_bytes()
    }

    /// Look up `text` in the cache; returns `None` on miss or poisoned lock.
    pub fn get(&self, text: &str) -> Option<Arc<Vec<f32>>> {
        let key = self.cache_key(text);
        let mut g = self.inner.lock().ok()?;
        g.get(&key).cloned()
    }

    /// Insert `value` for `text`, evicting the LRU entry if at capacity.
    /// Silently no-ops on a poisoned lock so the caller re-embeds on next access.
    pub fn put(&self, text: &str, value: Arc<Vec<f32>>) {
        let key = self.cache_key(text);
        if let Ok(mut g) = self.inner.lock() {
            g.put(key, value);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_returns_arc_eq() {
        let cache = EmbeddingCache::new("bge-small", 10);
        let vec = Arc::new(vec![1.0_f32, 2.0, 3.0]);
        cache.put("hello world", vec.clone());
        let result = cache.get("hello world").expect("should be cached");
        assert!(Arc::ptr_eq(&result, &vec), "must return the same Arc");
    }

    #[test]
    fn miss_after_capacity_eviction() {
        // Capacity 1: inserting a second entry evicts the first.
        let cache = EmbeddingCache::new("bge-small", 1);
        let v1 = Arc::new(vec![1.0_f32]);
        let v2 = Arc::new(vec![2.0_f32]);
        cache.put("first", v1);
        cache.put("second", v2);
        assert!(
            cache.get("first").is_none(),
            "first entry must be evicted after capacity overflow"
        );
        assert!(
            cache.get("second").is_some(),
            "second entry must still be present"
        );
    }

    #[test]
    fn different_models_dont_collide() {
        // Same text, different model_id → different cache key → no collision
        // when each cache is queried independently.
        let cache_a = EmbeddingCache::new("model-a", 10);
        let cache_b = EmbeddingCache::new("model-b", 10);
        let va = Arc::new(vec![1.0_f32]);
        let vb = Arc::new(vec![2.0_f32]);
        cache_a.put("text", va.clone());
        cache_b.put("text", vb.clone());
        let ra = cache_a.get("text").expect("model-a hit");
        let rb = cache_b.get("text").expect("model-b hit");
        assert!(Arc::ptr_eq(&ra, &va));
        assert!(Arc::ptr_eq(&rb, &vb));
        assert!(
            !Arc::ptr_eq(&ra, &rb),
            "different models must not share arcs"
        );
    }
}
