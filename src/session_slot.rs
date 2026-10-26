//! The session handle the internal API reads, plus whether the worker is reconnecting.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct SessionSlot<T> {
    slot: Arc<RwLock<Option<T>>>,
    reconnecting: Arc<AtomicBool>,
    reconnects: Arc<AtomicU64>,
}

impl<T> SessionSlot<T> {
    pub fn new(slot: Arc<RwLock<Option<T>>>) -> Self {
        Self {
            slot,
            reconnecting: Arc::new(AtomicBool::new(false)),
            reconnects: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The session went invalid: callers get no session until [Self::restored].
    pub fn lost(&self) {
        *self.slot.write().unwrap() = None;
        self.reconnecting.store(true, Ordering::Relaxed);
    }

    pub fn restored(&self, value: T) {
        *self.slot.write().unwrap() = Some(value);
        self.reconnecting.store(false, Ordering::Relaxed);
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear the reconnecting flag only (the slot and counter are left alone), for when the
    /// worker exits mid-reconnect.
    pub fn reset(&self) {
        self.reconnecting.store(false, Ordering::Relaxed);
    }

    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting.load(Ordering::Relaxed)
    }

    /// Reconnects since startup.
    #[cfg(any(test, feature = "harness"))]
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    #[test]
    fn lost_clears_the_slot_and_restored_fills_it() {
        let slot = SessionSlot::new(Arc::new(RwLock::new(Some(1u32))));
        slot.lost();
        assert_eq!(*slot.slot.read().unwrap(), None);
        assert!(slot.is_reconnecting());
        slot.restored(2);
        assert_eq!(*slot.slot.read().unwrap(), Some(2));
        assert!(!slot.is_reconnecting());
        assert_eq!(slot.reconnects(), 1);
    }

    #[test]
    fn a_failed_attempt_leaves_the_slot_empty() {
        let slot = SessionSlot::new(Arc::new(RwLock::new(Some(1u32))));
        slot.lost();
        slot.lost();
        assert_eq!(*slot.slot.read().unwrap(), None);
        assert_eq!(slot.reconnects(), 0);
    }

    #[test]
    fn reset_clears_only_the_reconnecting_flag() {
        let slot = SessionSlot::new(Arc::new(RwLock::new(Some(1u32))));
        slot.lost();
        slot.reset();
        assert!(!slot.is_reconnecting());
        assert_eq!(*slot.slot.read().unwrap(), None);
    }

    #[test]
    fn clones_share_state() {
        let slot = SessionSlot::new(Arc::new(RwLock::new(None::<u32>)));
        let other = slot.clone();
        other.lost();
        assert!(slot.is_reconnecting());
    }
}
