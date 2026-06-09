//! Reloadable lazy slot with idle-based unload.
//!
//! Backs `LazyFastEmbedReranker` (and any future lazily-loaded model
//! session): the value is created on first use, its last-used time is
//! tracked, and `unload_if_idle` drops it after a quiet period so the
//! next use transparently reloads it.

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
        assert_eq!(inits.load(Ordering::SeqCst), 1, "second call must not re-init");
    }

    #[tokio::test]
    async fn failed_init_is_retried() {
        let slot: IdleSlot<String> = IdleSlot::new();
        let r1: Result<String, String> = slot.get_or_try_init(async { Err("boom".to_string()) }).await;
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
