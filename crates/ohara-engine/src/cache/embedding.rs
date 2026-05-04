use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// LRU cache keyed by `blake3(model_id ":" text)` → shared embedding vector.
///
/// Thread-safe: wraps the inner [`LruCache`] in a [`Mutex`] so it can be
/// shared across async tasks via `Arc<EmbeddingCache>`.
pub struct EmbeddingCache {
    model_id: String,
    inner: Mutex<LruCache<[u8; 32], Arc<Vec<f32>>>>,
}

impl EmbeddingCache {
    pub fn new(_model_id: impl Into<String>, _capacity: usize) -> Self {
        todo!("EmbeddingCache::new not yet implemented")
    }

    pub fn get(&self, _text: &str) -> Option<Arc<Vec<f32>>> {
        todo!("EmbeddingCache::get not yet implemented")
    }

    pub fn put(&self, _text: &str, _value: Arc<Vec<f32>>) {
        todo!("EmbeddingCache::put not yet implemented")
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
