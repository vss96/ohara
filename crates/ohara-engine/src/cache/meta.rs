use ohara_core::query::ResponseMeta;
use ohara_core::types::RepoId;

/// TTL-bounded cache for [`ResponseMeta`] keyed by [`RepoId`].
///
/// Implementation is in progress — struct and methods are not yet defined.
pub struct MetaCache;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ohara_core::query::{IndexStatus, ResponseMeta};
    use std::time::Duration;

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
        assert!(cache.get(&key).is_some(), "must be present before invalidate");
        cache.invalidate(&key);
        assert!(cache.get(&key).is_none(), "must be absent after invalidate");
    }
}
