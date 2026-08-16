//! Mutex locking policy for the CLI.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, recovering the guard if a previous holder panicked.
///
/// The CLI recovers rather than propagating because the alternatives each
/// lose something concrete: `unwrap()` turns one panic into a cascade that
/// kills the `EpochTicker` and `serve`'s epoch thread, taking timeout
/// enforcement down with them, while `if let Ok(..)` / `unwrap_or_default()`
/// silently drop the update — in `kiln_provider`'s load recorder that would
/// hash an empty source list and produce a cache key that never invalidates.
///
/// Recovery is not free everywhere. Most of these mutexes hold a plain
/// collection, counter, or handle slot, whose data stays structurally valid
/// whatever a panicking holder was doing. Two do not: `TapReporter`'s
/// `TapDoc` interleaves `println!` with the bookkeeping that tracks the open
/// subtest, and `serve`'s profiler slot is mutated in place by `sample`. A
/// panic inside either — a broken pipe on stdout, say — leaves a recovered
/// guard rendering degraded output rather than failing loudly. That is the
/// accepted cost of keeping the run's timeout enforcement alive; anything
/// stronger belongs behind an assertion in the holder, not here.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
