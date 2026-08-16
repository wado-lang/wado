//! Mutex locking policy for the CLI.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering the guard if a previous holder panicked. Every
/// production mutex in the crate goes through here: propagating poison would
/// kill the `EpochTicker` and `serve`'s epoch thread, and with them timeout
/// enforcement.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
