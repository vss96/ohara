use ohara_core::query::ResponseMeta;
use ohara_core::types::RepoId;
use ohara_core::WorktreeKey;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cache key: one entry per worktree of a repository (plan-33), because
/// freshness (`commits_behind_head`) is per worktree.
type Key = (RepoId, WorktreeKey);

/// TTL-bounded cache for [`ResponseMeta`] keyed by `(RepoId, WorktreeKey)`.
///
/// Thread-safe: wraps the inner map in a [`Mutex`] so it can be shared
/// across async tasks via `Arc<MetaCache>`. Intended to memoize the `_meta`
/// response field for a short window (e.g. 5 s) to avoid hitting storage on
/// every MCP tool call.
pub struct MetaCache {
    ttl: Duration,
    inner: Mutex<HashMap<Key, (Instant, ResponseMeta)>>,
}

impl MetaCache {
    /// Create a new cache whose entries expire after `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached value for `(repo_id, worktree)` if it exists and is
    /// still within the TTL window. Returns `None` on a miss, an expired
    /// entry, or a poisoned lock.
    pub fn get(&self, repo_id: &RepoId, worktree: &WorktreeKey) -> Option<ResponseMeta> {
        let g = self.inner.lock().ok()?;
        let (t, v) = g.get(&(repo_id.clone(), worktree.clone()))?;
        if t.elapsed() > self.ttl {
            return None;
        }
        Some(v.clone())
    }

    /// Insert or overwrite the cached value for `(repo_id, worktree)`,
    /// stamping it with the current instant. Silently no-ops on a poisoned
    /// lock.
    pub fn put(&self, repo_id: RepoId, worktree: WorktreeKey, v: ResponseMeta) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert((repo_id, worktree), (Instant::now(), v));
        }
    }

    /// Remove the cached entries of every worktree of `repo_id`. Silently
    /// no-ops on a poisoned lock.
    pub fn invalidate(&self, repo_id: &RepoId) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|(r, _), _| r != repo_id);
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

    fn main_wt() -> WorktreeKey {
        WorktreeKey::main()
    }

    #[test]
    fn put_get_within_ttl_returns_value() {
        let cache = MetaCache::new(Duration::from_secs(5));
        let key = make_key();
        let meta = make_meta("hello");
        cache.put(key.clone(), main_wt(), meta.clone());
        let got = cache.get(&key, &main_wt()).unwrap();
        assert_eq!(got.hint.as_deref(), Some("hello"));
    }

    #[test]
    fn get_after_ttl_returns_none() {
        // Zero-duration TTL: every entry is immediately expired.
        let cache = MetaCache::new(Duration::from_nanos(0));
        let key = make_key();
        cache.put(key.clone(), main_wt(), make_meta("stale"));
        // Spin briefly so elapsed() > 0 ns.
        std::thread::sleep(Duration::from_millis(1));
        assert!(
            cache.get(&key, &main_wt()).is_none(),
            "expired entry must return None"
        );
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = MetaCache::new(Duration::from_secs(60));
        let key = make_key();
        cache.put(key.clone(), main_wt(), make_meta("present"));
        assert!(
            cache.get(&key, &main_wt()).is_some(),
            "must be present before invalidate"
        );
        cache.invalidate(&key);
        assert!(
            cache.get(&key, &main_wt()).is_none(),
            "must be absent after invalidate"
        );
    }

    #[test]
    fn worktrees_do_not_share_entries_and_invalidate_drops_all() {
        let cache = MetaCache::new(Duration::from_secs(60));
        let key = make_key();
        let slot = WorktreeKey::linked("slot-1").unwrap();
        cache.put(key.clone(), main_wt(), make_meta("main"));
        assert!(cache.get(&key, &slot).is_none(), "must not cross worktrees");
        cache.put(key.clone(), slot.clone(), make_meta("slot"));
        cache.invalidate(&key);
        assert!(cache.get(&key, &main_wt()).is_none());
        assert!(cache.get(&key, &slot).is_none());
    }
}
