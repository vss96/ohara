//! Reloadable lazy slot with idle-based unload.
//!
//! Backs `LazyFastEmbedReranker` (and any future lazily-loaded model
//! session): the value is created on first use, its last-used time is
//! tracked, and `unload_if_idle` drops it after a quiet period so the
//! next use transparently reloads it.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

// plan-29: consumed by LazyFastEmbedReranker in the next commit
pub(crate) struct IdleSlot<T> {
    slot: RwLock<Option<T>>,
    /// Unix-seconds of the most recent `get_or_try_init` call; 0 = never used.
    last_used_unix: AtomicU64,
}

#[allow(dead_code)]
impl<T: Clone> IdleSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            slot: RwLock::new(None),
            last_used_unix: AtomicU64::new(0),
        }
    }

    /// Return the value, initialising via `init` when the slot is empty.
    /// Refreshes the last-used stamp. Failed inits leave the slot empty,
    /// so the next call retries.
    pub(crate) async fn get_or_try_init<E>(
        &self,
        init: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        self.last_used_unix.store(now_unix(), Ordering::Relaxed);
        if let Some(v) = self.slot.read().await.clone() {
            return Ok(v);
        }
        let mut guard = self.slot.write().await;
        // Double-check: another task may have initialised while we
        // waited for the write lock.
        if let Some(v) = guard.clone() {
            return Ok(v);
        }
        let v = init.await?;
        *guard = Some(v.clone());
        Ok(v)
    }

    /// Drop the value when it has been idle for at least `idle`.
    /// Returns `true` when a value was dropped.
    pub(crate) async fn unload_if_idle(&self, idle: Duration) -> bool {
        let last = self.last_used_unix.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        if now_unix().saturating_sub(last) < idle.as_secs() {
            return false;
        }
        let mut guard = self.slot.write().await;
        if guard.is_none() {
            return false;
        }
        *guard = None;
        true
    }

    #[cfg(test)]
    pub(crate) fn force_last_used(&self, unix: u64) {
        self.last_used_unix.store(unix, Ordering::Relaxed);
    }
}

// plan-29: consumed by LazyFastEmbedReranker in the next commit
#[allow(dead_code)]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn initialises_once_and_reuses_value() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let inits = AtomicUsize::new(0);
        let v1: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("model".to_string())
            })
            .await;
        let v2: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("model".to_string())
            })
            .await;
        assert_eq!(v1.unwrap(), "model");
        assert_eq!(v2.unwrap(), "model");
        assert_eq!(
            inits.load(Ordering::SeqCst),
            1,
            "second call must not re-init"
        );
    }

    #[tokio::test]
    async fn failed_init_is_retried() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let r1: Result<String, String> = slot
            .get_or_try_init(async { Err("boom".to_string()) })
            .await;
        assert!(r1.is_err());
        let r2: Result<String, String> = slot
            .get_or_try_init(async { Ok("recovered".to_string()) })
            .await;
        assert_eq!(r2.unwrap(), "recovered");
    }

    #[tokio::test]
    async fn unload_on_empty_slot_is_false() {
        let slot: IdleSlot<String> = IdleSlot::new();
        assert!(!slot.unload_if_idle(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn unload_after_idle_drops_and_reload_reinits() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let _: Result<String, ()> = slot.get_or_try_init(async { Ok("v1".to_string()) }).await;
        // Fresh value: a 1-hour idle threshold must NOT unload it.
        assert!(!slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Force the last-used stamp into the past, then unload.
        slot.force_last_used(1);
        assert!(slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Unloading twice is false (already empty).
        slot.force_last_used(1);
        assert!(!slot.unload_if_idle(Duration::from_secs(3600)).await);
        // Next get re-initialises.
        let inits = AtomicUsize::new(0);
        let v: Result<String, ()> = slot
            .get_or_try_init(async {
                inits.fetch_add(1, Ordering::SeqCst);
                Ok("v2".to_string())
            })
            .await;
        assert_eq!(v.unwrap(), "v2");
        assert_eq!(inits.load(Ordering::SeqCst), 1);
    }
}
