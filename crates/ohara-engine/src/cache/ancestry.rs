use async_trait::async_trait;
use lru::LruCache;
use ohara_core::{CommitSha, CommitVisibility, OhraError};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Default number of `(head, commit)` verdicts kept.
const DEFAULT_CAPACITY: usize = 65_536;

/// LRU of `(head, commit)` pairs known to be visible. Only `true` verdicts
/// are kept: once `commit` is an ancestor of `head` it stays one. A `false`
/// verdict can change (a commit object missing from a shallow or partial
/// clone may be fetched later, and two clones sharing one engine-wide cache
/// can hold different objects), so it is asked again every time.
///
/// Thread-safe: the map sits behind a [`Mutex`] so one cache can be shared
/// by every repo of an engine (keys hold full SHAs).
pub struct AncestryCache {
    inner: Mutex<LruCache<(CommitSha, CommitSha), bool>>,
    hits: AtomicU64,
}

impl AncestryCache {
    /// Create a cache holding at most `capacity` verdicts (minimum 1).
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            hits: AtomicU64::new(0),
        }
    }

    /// Number of commit lookups answered from the cache so far.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cached verdicts for `commits` as seen from `head`; `None` marks a miss.
    fn lookup(&self, head: &CommitSha, commits: &[CommitSha]) -> Vec<Option<bool>> {
        let Ok(mut g) = self.inner.lock() else {
            return vec![None; commits.len()];
        };
        let found: Vec<Option<bool>> = commits
            .iter()
            .map(|c| g.get(&(head.clone(), c.clone())).copied())
            .collect();
        let n = found.iter().flatten().count() as u64;
        self.hits.fetch_add(n, Ordering::Relaxed);
        found
    }

    /// Record the `true` verdicts; silently no-ops on a poisoned lock.
    fn store(&self, head: &CommitSha, commits: &[CommitSha], verdicts: &[bool]) {
        if let Ok(mut g) = self.inner.lock() {
            for (c, v) in commits.iter().zip(verdicts) {
                if *v {
                    g.put((head.clone(), c.clone()), true);
                }
            }
        }
    }
}

impl Default for AncestryCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// Decorator over a [`CommitVisibility`] oracle: answers from the cache and
/// asks `inner` once, for the misses only.
pub struct CachedVisibility {
    inner: Arc<dyn CommitVisibility>,
    cache: Arc<AncestryCache>,
}

impl CachedVisibility {
    /// Wrap `inner` with `cache`.
    pub fn new(inner: Arc<dyn CommitVisibility>, cache: Arc<AncestryCache>) -> Self {
        Self { inner, cache }
    }
}

#[async_trait]
impl CommitVisibility for CachedVisibility {
    async fn visible(
        &self,
        head: &CommitSha,
        commits: &[CommitSha],
    ) -> ohara_core::Result<Vec<bool>> {
        let mut out = self.cache.lookup(head, commits);
        let miss_idx: Vec<usize> = (0..out.len()).filter(|i| out[*i].is_none()).collect();
        if miss_idx.is_empty() {
            return Ok(out.into_iter().flatten().collect());
        }
        let misses: Vec<CommitSha> = miss_idx.iter().map(|i| commits[*i].clone()).collect();
        let answers = self.inner.visible(head, &misses).await?;
        if answers.len() != misses.len() {
            return Err(OhraError::Git(format!(
                "visibility oracle returned {} answers for {} commits",
                answers.len(),
                misses.len()
            )));
        }
        self.cache.store(head, &misses, &answers);
        for (i, v) in miss_idx.into_iter().zip(answers) {
            out[i] = Some(v);
        }
        Ok(out.into_iter().flatten().collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ohara_core::{CommitSha, CommitVisibility};
    use std::sync::{Arc, Mutex};

    /// Records every batch it is asked about; a commit is visible when its
    /// sha starts with `a`.
    struct CountingOracle {
        calls: Mutex<Vec<Vec<CommitSha>>>,
    }

    #[async_trait]
    impl CommitVisibility for CountingOracle {
        async fn visible(
            &self,
            _head: &CommitSha,
            commits: &[CommitSha],
        ) -> ohara_core::Result<Vec<bool>> {
            self.calls.lock().unwrap().push(commits.to_vec());
            Ok(commits
                .iter()
                .map(|c| c.as_str().starts_with('a'))
                .collect())
        }
    }

    fn sha(c: char) -> CommitSha {
        CommitSha::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn setup() -> (Arc<CountingOracle>, Arc<AncestryCache>, CachedVisibility) {
        let inner = Arc::new(CountingOracle {
            calls: Mutex::new(Vec::new()),
        });
        let cache = Arc::new(AncestryCache::new(16));
        let cached = CachedVisibility::new(inner.clone(), cache.clone());
        (inner, cache, cached)
    }

    #[tokio::test]
    async fn second_lookup_serves_visible_verdicts_from_cache() {
        let (inner, cache, cached) = setup();
        let head = sha('f');
        let q = [sha('a'), sha('b')];
        assert_eq!(cached.visible(&head, &q).await.unwrap(), vec![true, false]);
        assert_eq!(cached.visible(&head, &q).await.unwrap(), vec![true, false]);
        let calls = inner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 2);
        assert_eq!(
            calls[1],
            vec![sha('b')],
            "only the false verdict is asked again"
        );
        assert_eq!(cache.hits(), 1);
    }

    #[tokio::test]
    async fn only_misses_reach_the_inner_oracle() {
        let (inner, _cache, cached) = setup();
        let head = sha('f');
        cached.visible(&head, &[sha('a')]).await.unwrap();
        let out = cached.visible(&head, &[sha('b'), sha('a')]).await.unwrap();
        assert_eq!(out, vec![false, true], "input order preserved");
        let calls = inner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], vec![sha('b')]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod false_verdict_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Says "not visible" until `fetched` is set, as for a commit object a
    /// shallow clone lacks until it is fetched.
    struct Fetchable {
        fetched: AtomicBool,
    }

    #[async_trait]
    impl CommitVisibility for Fetchable {
        async fn visible(
            &self,
            _head: &CommitSha,
            commits: &[CommitSha],
        ) -> ohara_core::Result<Vec<bool>> {
            let v = self.fetched.load(Ordering::SeqCst);
            Ok(vec![v; commits.len()])
        }
    }

    #[tokio::test]
    async fn a_false_verdict_is_not_cached_because_it_can_change() {
        let head = CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let c = CommitSha::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let oracle = Arc::new(Fetchable {
            fetched: AtomicBool::new(false),
        });
        let cached = CachedVisibility::new(oracle.clone(), Arc::new(AncestryCache::new(16)));
        assert_eq!(
            cached
                .visible(&head, std::slice::from_ref(&c))
                .await
                .unwrap(),
            vec![false]
        );
        oracle.fetched.store(true, Ordering::SeqCst);
        assert_eq!(
            cached
                .visible(&head, std::slice::from_ref(&c))
                .await
                .unwrap(),
            vec![true],
            "a missing object fetched later must become visible"
        );
        // A true verdict never changes, so it is served from the cache.
        oracle.fetched.store(false, Ordering::SeqCst);
        assert_eq!(cached.visible(&head, &[c]).await.unwrap(), vec![true]);
    }
}
