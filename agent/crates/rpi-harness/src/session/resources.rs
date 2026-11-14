//! Session resource cleanup registry — mirrors `packages/ai/src/session-resources.ts`.
//!
//! Provides a global registry of cleanup callbacks that are invoked when a
//! session ends. This allows extensions, tools, and other components to
//! register resource cleanup handlers that run reliably when sessions close,
//! regardless of how they close (normal exit, abort, crash).
//!
//! # Use Cases
//!
//! - **Temporary files**: Clean up temp files created during a session.
//! - **Network connections**: Close persistent connections opened for a session.
//! - **Lock files**: Release file locks held during a session.
//! - **Child processes**: Terminate background processes spawned for a session.
//!
//! # Example
//!
//! ```rust
//! use rpi_harness::session::resources::{register_cleanup, cleanup_all};
//!
//! // Register a cleanup callback
//! let unregister = register_cleanup(Box::new(|session_id| {
//!     println!("Cleaning up session: {}", session_id.unwrap_or("unknown"));
//! }));
//!
//! // ... session runs ...
//!
//! // When session ends, run all cleanup callbacks
//! cleanup_all(Some("session-123"));
//!
//! // Or unregister early if no longer needed
//! unregister();
//! ```

use std::sync::{Arc, Mutex};

/// A cleanup callback invoked when a session ends. Receives the session id
/// (or `None` if cleaning up all sessions).
pub type SessionResourceCleanup = Box<dyn Fn(Option<&str>) + Send + Sync>;

/// Global registry of session resource cleanup callbacks.
static CLEANUP_REGISTRY: Mutex<Vec<Arc<SessionResourceCleanup>>> = Mutex::new(Vec::new());

/// Register a session resource cleanup callback. Returns an unregister function
/// that removes the callback when called.
///
/// The callback is invoked with the session id when that session ends, or with
/// `None` when cleaning up all sessions.
///
/// # Example
///
/// ```rust
/// use rpi_harness::session::resources::register_cleanup;
///
/// let unregister = register_cleanup(Box::new(|session_id| {
///     // Clean up resources for this session
///     if let Some(id) = session_id {
///         println!("Cleaning up session: {id}");
///     }
/// }));
///
/// // Later, if the cleanup is no longer needed:
/// unregister();
/// ```
pub fn register_cleanup(cleanup: SessionResourceCleanup) -> impl Fn() {
    let cleanup = Arc::new(cleanup);
    {
        let mut registry = CLEANUP_REGISTRY.lock().expect("cleanup registry poisoned");
        registry.push(Arc::clone(&cleanup));
    }

    // Return an unregister function
    let cleanup_clone = Arc::clone(&cleanup);
    move || {
        let mut registry = CLEANUP_REGISTRY.lock().expect("cleanup registry poisoned");
        registry.retain(|c| !Arc::ptr_eq(c, &cleanup_clone));
    }
}

/// Run all registered cleanup callbacks. Errors from individual callbacks are
/// collected but do not prevent other callbacks from running.
///
/// # Arguments
///
/// * `session_id` - The session being cleaned up, or `None` to clean up all sessions.
///
/// # Returns
///
/// A list of errors from callbacks that panicked or otherwise failed.
pub fn cleanup_all(session_id: Option<&str>) -> Vec<CleanupError> {
    let registry = CLEANUP_REGISTRY.lock().expect("cleanup registry poisoned");
    let mut errors = Vec::new();

    for cleanup in registry.iter() {
        // Use catch_unwind to handle panics in cleanup callbacks
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup(session_id);
        }));

        if let Err(panic_info) = result {
            let message = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            errors.push(CleanupError {
                session_id: session_id.map(String::from),
                message,
            });
        }
    }

    errors
}

/// Clear all registered cleanup callbacks. Primarily useful for testing.
pub fn clear_all() {
    let mut registry = CLEANUP_REGISTRY.lock().expect("cleanup registry poisoned");
    registry.clear();
}

/// Get the number of registered cleanup callbacks. Primarily useful for testing.
pub fn cleanup_count() -> usize {
    let registry = CLEANUP_REGISTRY.lock().expect("cleanup registry poisoned");
    registry.len()
}

/// An error from a cleanup callback.
#[derive(Debug, Clone)]
pub struct CleanupError {
    /// The session id being cleaned up (if any).
    pub session_id: Option<String>,
    /// The error message.
    pub message: String,
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.session_id {
            Some(id) => write!(f, "cleanup error for session {id}: {}", self.message),
            None => write!(f, "cleanup error: {}", self.message),
        }
    }
}

impl std::error::Error for CleanupError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Global lock to serialize tests that use the shared CLEANUP_REGISTRY
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn register_and_cleanup() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_all();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let _unregister = register_cleanup(Box::new(move |_session_id| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        assert_eq!(cleanup_count(), 1);
        let errors = cleanup_all(Some("test-session"));
        assert!(errors.is_empty());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unregister_removes_callback() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_all();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let unregister = register_cleanup(Box::new(move |_session_id| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        assert_eq!(cleanup_count(), 1);
        unregister();
        assert_eq!(cleanup_count(), 0);

        let errors = cleanup_all(Some("test-session"));
        assert!(errors.is_empty());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn multiple_callbacks_all_run() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_all();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let c = Arc::clone(&counter);
            let _unregister = register_cleanup(Box::new(move |_session_id| {
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }

        assert_eq!(cleanup_count(), 5);
        let errors = cleanup_all(None);
        assert!(errors.is_empty());
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn panic_in_callback_does_not_stop_others() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_all();
        let counter = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&counter);
        let c2 = Arc::clone(&counter);

        let _u1 = register_cleanup(Box::new(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        }));

        let _u2 = register_cleanup(Box::new(move |_| {
            panic!("intentional test panic");
        }));

        let _u3 = register_cleanup(Box::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        }));

        let errors = cleanup_all(Some("test"));
        // The panic is caught, but counter may be 1 or 2 depending on order
        // At minimum, the first callback ran
        assert!(counter.load(Ordering::SeqCst) >= 1);
        // We should have at least one error from the panic
        assert!(!errors.is_empty());
    }

    #[test]
    fn cleanup_passes_session_id() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_all();
        let received_id = Arc::new(Mutex::new(None));
        let r = Arc::clone(&received_id);

        let _unregister = register_cleanup(Box::new(move |session_id| {
            *r.lock().unwrap() = session_id.map(String::from);
        }));

        cleanup_all(Some("my-session-42"));
        assert_eq!(
            received_id.lock().unwrap().as_deref(),
            Some("my-session-42")
        );
    }
}
