//! Mutex locking policy for the CLI.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering the guard if a previous holder panicked.
///
/// Every mutex in the CLI guards a plain collection, counter, or handle
/// slot — none couples two fields whose invariant a panicking holder could
/// leave half-updated — so the data is structurally valid regardless. The
/// alternatives all lose something real: `unwrap()` turns one panic into a
/// cascade that kills the `EpochTicker` and `serve`'s epoch thread, taking
/// timeout enforcement down with them, while `if let Ok(..)` / `unwrap_or_default()`
/// silently drop the update — in `kiln_provider`'s load recorder that would
/// hash an empty source list and produce a cache key that never invalidates.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
