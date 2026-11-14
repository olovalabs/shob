//! Safe storage for plugin-owned tool state behind opaque ABI handles.
//!
//! Handles are non-reused numeric tokens, never addresses to dereference.
//! Each plugin owns its registry; Rust objects and locks stay on its side of
//! the C ABI. Removing a handle cancels it and prevents future lookups, while
//! in-flight callbacks retain their own `Arc` until they return.
#![forbid(unsafe_code)]

use crate::StepHandle;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(1);

pub struct ToolHandles<T> {
    entries: Mutex<Option<HashMap<usize, Arc<ToolState<T>>>>>,
}

pub struct ToolState<T> {
    state: Mutex<T>,
    pub cancelled: Arc<AtomicBool>,
}

impl<T> ToolState<T> {
    /// Serialize mutations without holding the registry lock.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Cancellation never waits for a potentially blocking poll.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl<T> Default for ToolHandles<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ToolHandles<T> {
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(None),
        }
    }

    pub fn insert(&self, state: T) -> StepHandle {
        self.insert_with_cancel(state, Arc::new(AtomicBool::new(false)))
    }

    /// Share the cancellation flag with a worker started by the plugin.
    pub fn insert_with_cancel(&self, state: T, cancelled: Arc<AtomicBool>) -> StepHandle {
        // Use a CAS loop to retain MSRV compatibility without deprecated fetch_update.
        let mut token = NEXT_HANDLE.load(Ordering::Relaxed);
        loop {
            let next = token.checked_add(1).expect("tool handle tokens exhausted");
            match NEXT_HANDLE.compare_exchange_weak(
                token,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => token = current,
            }
        }
        let entry = Arc::new(ToolState {
            state: Mutex::new(state),
            cancelled,
        });
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(token, entry);
        token as StepHandle
    }

    /// Unknown, null, removed, or another registry's handles return `None`.
    pub fn get(&self, handle: StepHandle) -> Option<Arc<ToolState<T>>> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()?
            .get(&(handle as usize))
            .cloned()
    }

    pub fn cancel(&self, handle: StepHandle) {
        if let Some(entry) = self.get(handle) {
            entry.cancel();
        }
    }

    /// Idempotently retire the token. Drops happen outside the registry lock.
    pub fn remove(&self, handle: StepHandle) -> Option<Arc<ToolState<T>>> {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()?
            .remove(&(handle as usize));
        if let Some(entry) = &entry {
            entry.cancel();
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_retired_and_foreign_handles_are_rejected() {
        let handles = ToolHandles::new();
        let other = ToolHandles::new();
        let first = handles.insert(1);
        let second = other.insert(2);
        assert!(handles.get(std::ptr::null_mut()).is_none());
        assert!(handles.get(usize::MAX as StepHandle).is_none());
        assert!(handles.get(second).is_none());
        assert!(handles.remove(first).is_some());
        assert!(handles.remove(first).is_none());
        assert!(handles.get(first).is_none());
        assert_ne!(handles.insert(3), first);
    }

    #[test]
    fn concurrent_insertions_allocate_unique_nonzero_handles() {
        let handles = ToolHandles::new();
        let tokens = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let handles = &handles;
                    scope.spawn(move || {
                        (0..100)
                            .map(|value| handles.insert(value) as usize)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let unique: std::collections::HashSet<_> = tokens.iter().copied().collect();
        assert_eq!(unique.len(), 800);
        assert!(!unique.contains(&0));
        for token in tokens {
            assert!(handles.get(token as StepHandle).is_some());
        }
    }

    #[test]
    fn cancellation_does_not_wait_for_the_state_lock() {
        let handles = ToolHandles::new();
        let handle = handles.insert(0);
        let entry = handles.get(handle).unwrap();
        let _guard = entry.lock();
        let token = handle as usize;
        std::thread::scope(|scope| {
            scope
                .spawn(|| handles.cancel(token as StepHandle))
                .join()
                .unwrap();
        });
        assert!(entry.cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn removal_keeps_in_flight_state_alive_and_drops_it_once() {
        struct CountDrop(Arc<AtomicUsize>);
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let handles = ToolHandles::new();
        let handle = handles.insert(CountDrop(drops.clone()));
        let entry = handles.get(handle).unwrap();
        drop(handles.remove(handle));
        assert!(entry.cancelled.load(Ordering::SeqCst));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(entry);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(handles.remove(handle).is_none());
    }

    #[test]
    fn concurrent_polls_serialize_state_changes() {
        let handles = ToolHandles::new();
        let handle = handles.insert(0);
        let entry = handles.get(handle).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let entry = entry.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        *entry.lock() += 1;
                    }
                });
            }
        });
        assert_eq!(*entry.lock(), 400);
    }

    #[test]
    fn removal_signals_the_workers_shared_cancel_flag() {
        let handles = ToolHandles::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = handles.insert_with_cancel((), cancelled.clone());
        drop(handles.remove(handle));
        assert!(cancelled.load(Ordering::SeqCst));
    }
}
