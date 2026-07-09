//! Cooperative cancellation for long-running git subprocess calls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared cancellation flag. Cloning shares the same underlying flag, so a UI
/// thread can hold one handle and signal cancellation while a background thread
/// drives a `git push`/`fetch` with another.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. The next checkpoint in a running operation will
    /// kill the child process and return [`gg_core::GitError::Cancelled`].
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Clear the flag so the (shared) token can be reused for a fresh
    /// operation. Callers reset before starting work, not while it runs.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
