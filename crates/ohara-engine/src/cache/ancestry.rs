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
    async fn second_lookup_is_served_from_cache() {
        let (inner, cache, cached) = setup();
        let head = sha('f');
        let q = [sha('a'), sha('b')];
        assert_eq!(cached.visible(&head, &q).await.unwrap(), vec![true, false]);
        assert_eq!(cached.visible(&head, &q).await.unwrap(), vec![true, false]);
        let calls = inner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "inner must be asked once");
        assert_eq!(calls[0].len(), 2);
        assert_eq!(cache.hits(), 2);
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
