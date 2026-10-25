use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

struct Entry {
    lock: Arc<AsyncMutex<()>>,
    holders: usize,
}

pub(crate) struct KeyedLocks<K: Ord + Clone> {
    entries: Arc<Mutex<BTreeMap<K, Entry>>>,
}

impl<K: Ord + Clone> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl<K: Ord + Clone> KeyedLocks<K> {
    pub async fn lock(&self, key: K) -> KeyedGuard<K> {
        let lock = {
            let mut entries = self.entries.lock().unwrap_or_else(dvb_core::sync::recover);
            let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
                lock: Arc::new(AsyncMutex::new(())),
                holders: 0,
            });
            entry.holders += 1;
            entry.lock.clone()
        };
        // Register before awaiting, so a queued or cancelled waiter owns its entry.
        let holder = Holder {
            entries: self.entries.clone(),
            key,
        };
        KeyedGuard {
            _guard: lock.lock_owned().await,
            _holder: holder,
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }
}

struct Holder<K: Ord + Clone> {
    entries: Arc<Mutex<BTreeMap<K, Entry>>>,
    key: K,
}

impl<K: Ord + Clone> Drop for Holder<K> {
    fn drop(&mut self) {
        let mut entries = self.entries.lock().unwrap_or_else(dvb_core::sync::recover);
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.holders -= 1;
            if entry.holders == 0 {
                entries.remove(&self.key);
            }
        }
    }
}

#[must_use = "the keyed lock is held only while this guard is kept"]
pub struct KeyedGuard<K: Ord + Clone> {
    // Release the mutex before pruning; field drop order is significant.
    _guard: OwnedMutexGuard<()>,
    _holder: Holder<K>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;

    #[tokio::test]
    async fn contention_preserves_queued_waiters_and_independent_keys() {
        let locks = KeyedLocks::default();
        let first = locks.lock("a").await;
        let mut second = Box::pin(locks.lock("a"));
        assert!(poll!(&mut second).is_pending());
        let independent = locks.lock("b").await;
        drop(independent);
        assert_eq!(locks.entries.lock().unwrap().len(), 1);
        drop(first);
        let mut third = Box::pin(locks.lock("a"));
        assert!(poll!(&mut third).is_pending());
        let second = second.await;
        assert!(poll!(&mut third).is_pending());
        drop(second);
        drop(third.await);
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn cancelling_waiters_before_and_after_wake_prunes_the_last_holder() {
        for wake in [false, true] {
            let locks = KeyedLocks::default();
            let first = locks.lock("a").await;
            let mut cancelled = Box::pin(locks.lock("a"));
            assert!(poll!(&mut cancelled).is_pending());
            let mut survivor = Box::pin(locks.lock("a"));
            assert!(poll!(&mut survivor).is_pending());
            if wake {
                drop(first);
                assert!(!locks.is_empty());
                drop(cancelled);
            } else {
                drop(cancelled);
                assert!(poll!(&mut survivor).is_pending());
                drop(first);
            }
            drop(survivor.await);
            assert!(locks.is_empty());
        }
        let locks = KeyedLocks::default();
        let first = locks.lock("a").await;
        let mut cancelled = Box::pin(locks.lock("a"));
        assert!(poll!(&mut cancelled).is_pending());
        drop(first);
        drop(cancelled);
        assert!(locks.is_empty());
    }
}
