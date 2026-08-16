//! Mutex locking policy for the CLI.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering the guard if a previous holder panicked.
///
/// The CLI recovers because both alternatives lose something concrete.
/// `unwrap()` turns one panic into a cascade that kills the `EpochTicker`
/// and `serve`'s epoch thread, taking timeout enforcement with them;
/// `if let Ok(..)` / `unwrap_or_default()` silently drop the update — in
/// `kiln_provider`'s load recorder that would hash an empty source list into
/// a generator cache key that never invalidates.
///
/// Most of these mutexes hold a plain collection, counter, or handle slot,
/// which stays structurally valid whatever a panicking holder was doing. The
/// exceptions are the ones a holder mutates in place while doing something
/// that can panic: the `GuestProfiler` slots `sample` writes through (`run`
/// and `serve` under `--profile guest`), and `TapDoc`, which interleaves
/// `println!` with the bookkeeping tracking the open subtest. A panic there —
/// a broken pipe on stdout, say — leaves a recovered guard emitting a
/// degraded profile or TAP document instead of failing loudly. That is the
/// accepted cost; a holder needing more belongs behind its own assertion.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
