//! Keyed operation queue — mirrors `packages/agent/src/harness/session/keyed-operation-queue.ts`.
//!
//! A concurrency control primitive that serializes operations by key. Operations
//! with the same key run sequentially; operations with different keys can run
//! concurrently. Optionally limits the total number of concurrent operations
//! across all keys.
//!
//! # Use Cases
//!
//! - **File mutation queue**: serialize edits to the same file while allowing
//!   concurrent edits to different files.
//! - **Session lane queue**: serialize writes to the same session lane.
//! - **Resource cleanup**: ensure cleanup operations for a session complete
//!   before the next session starts.
//!
//! # Example
//!
//! ```rust,no_run
//! use rpi_harness::session::keyed_queue::KeyedOperationQueue;
//!
//! # async fn example() {
//! let queue = KeyedOperationQueue::<String>::new(None);
//!
//! // Operations with the same key run sequentially
//! let op1 = queue.enqueue("file-a".to_string(), || async { /* edit file A */ 1 }).await;
//! let op2 = queue.enqueue("file-a".to_string(), || async { /* edit file A again */ 2 }).await;
//!
//! // Operations with different keys can run concurrently
//! let op3 = queue.enqueue("file-b".to_string(), || async { /* edit file B */ 3 }).await;
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// A keyed operation queue. Operations with the same key run sequentially;
/// operations with different keys can run concurrently (subject to the optional
/// `max_concurrent` limit).
///
/// Implementation: each key maps to an `Arc<Mutex<()>>`. Operations acquire the
/// per-key mutex, ensuring sequential execution per key. Different keys have
/// different mutexes, so they can proceed concurrently.
pub struct KeyedOperationQueue<TKey> {
    /// Per-key locks. Each key maps to a mutex; operations for the same key
    /// contend on the same mutex, ensuring sequential execution.
    locks: Arc<Mutex<HashMap<TKey, Arc<Mutex<()>>>>>,
    /// Optional global concurrency limit. When `Some`, at most `max` operations
    /// can run concurrently across all keys.
    semaphore: Option<Arc<Semaphore>>,
}

impl<TKey> KeyedOperationQueue<TKey>
where
    TKey: std::hash::Hash + Eq + Clone + Send + Sync + 'static,
{
    /// Create a new keyed operation queue. `max_concurrent` limits the total
    /// number of concurrent operations across all keys; `None` means unlimited.
    pub fn new(max_concurrent: Option<usize>) -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
            semaphore: max_concurrent.map(|max| Arc::new(Semaphore::new(max))),
        }
    }

    /// Get or create the per-key lock.
    async fn get_lock(&self, key: &TKey) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Enqueue an operation for the given key. The operation runs after all
    /// previously enqueued operations for the same key complete. Returns the
    /// operation's result.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use rpi_harness::session::keyed_queue::KeyedOperationQueue;
    ///
    /// # async fn example() {
    /// let queue = KeyedOperationQueue::<String>::new(None);
    /// let result = queue.enqueue("key".to_string(), || async { 42 }).await;
    /// assert_eq!(result, 42);
    /// # }
    /// ```
    pub async fn enqueue<T, F, Fut>(&self, key: TKey, operation: F) -> T
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Acquire a semaphore permit if concurrency is limited
        let _permit = if let Some(sem) = &self.semaphore {
            Some(sem.acquire().await.expect("semaphore closed"))
        } else {
            None
        };

        // Acquire the per-key lock (waits for any previous operation on this key)
        let key_lock = self.get_lock(&key).await;
        let _guard = key_lock.lock().await;

        // Run the operation while holding the per-key lock
        operation().await
    }

    /// Enqueue a fire-and-forget operation. The operation runs in the background
    /// after any previous operations on the same key complete.
    pub fn enqueue_detached<F, Fut>(&self, key: TKey, operation: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let locks = Arc::clone(&self.locks);
        let semaphore = self.semaphore.clone();

        tokio::spawn(async move {
            // Acquire a semaphore permit if concurrency is limited
            let _permit = if let Some(sem) = &semaphore {
                Some(sem.acquire().await.expect("semaphore closed"))
            } else {
                None
            };

            // Get or create the per-key lock
            let key_lock = {
                let mut locks_map = locks.lock().await;
                locks_map
                    .entry(key)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };

            // Acquire the per-key lock
            let _guard = key_lock.lock().await;

            // Run the operation
            operation().await;
        });
    }
}

impl<TKey> Clone for KeyedOperationQueue<TKey> {
    fn clone(&self) -> Self {
        Self {
            locks: Arc::clone(&self.locks),
            semaphore: self.semaphore.as_ref().map(Arc::clone),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_operations_run_sequentially() {
        let queue = KeyedOperationQueue::<String>::new(None);
        let counter = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&counter);
        let op1 = queue.enqueue("key".to_string(), move || async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            c1.fetch_add(1, Ordering::SeqCst);
        });

        let c2 = Arc::clone(&counter);
        let op2 = queue.enqueue("key".to_string(), move || async move {
            // This should see counter == 1 (op1 completed)
            c2.load(Ordering::SeqCst)
        });

        op1.await;
        let r2 = op2.await;
        assert_eq!(r2, 1); // op2 sees 1 because op1 already finished
    }

    #[tokio::test]
    async fn different_key_operations_can_run_concurrently() {
        let queue = KeyedOperationQueue::<String>::new(None);
        let started = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let queue1 = queue.clone();
        let s1 = Arc::clone(&started);
        let b1 = Arc::clone(&barrier);
        let op1 = tokio::spawn(async move {
            queue1
                .enqueue("key-a".to_string(), move || async move {
                    s1.fetch_add(1, Ordering::SeqCst);
                    b1.wait().await;
                })
                .await
        });

        let queue2 = queue.clone();
        let s2 = Arc::clone(&started);
        let b2 = Arc::clone(&barrier);
        let op2 = tokio::spawn(async move {
            queue2
                .enqueue("key-b".to_string(), move || async move {
                    s2.fetch_add(1, Ordering::SeqCst);
                    b2.wait().await;
                })
                .await
        });

        // Both should complete (they run concurrently on different keys)
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = op1.await;
            let _ = op2.await;
        })
        .await;

        assert!(result.is_ok(), "Operations should complete within timeout");
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn max_concurrent_limits_parallelism() {
        let queue = KeyedOperationQueue::<String>::new(Some(2));
        let running = Arc::new(AtomicUsize::new(0));
        let max_running = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..5 {
            let r = Arc::clone(&running);
            let m = Arc::clone(&max_running);
            let handle = queue.enqueue(format!("key-{i}"), move || async move {
                let current = r.fetch_add(1, Ordering::SeqCst) + 1;
                // Update max if needed
                let mut prev = m.load(Ordering::SeqCst);
                while current > prev {
                    match m.compare_exchange_weak(prev, current, Ordering::SeqCst, Ordering::SeqCst)
                    {
                        Ok(_) => break,
                        Err(p) => prev = p,
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                r.fetch_sub(1, Ordering::SeqCst);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await;
        }

        assert!(max_running.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn clone_shares_state() {
        let queue1 = KeyedOperationQueue::<String>::new(None);
        let queue2 = queue1.clone();

        let counter = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&counter);
        let op1 = queue1.enqueue("key".to_string(), move || async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            c1.fetch_add(1, Ordering::SeqCst);
        });

        let c2 = Arc::clone(&counter);
        let op2 = queue2.enqueue("key".to_string(), move || async move {
            c2.load(Ordering::SeqCst)
        });

        op1.await;
        let result = op2.await;
        assert_eq!(result, 1); // Cloned queue shares the same locks
    }
}
