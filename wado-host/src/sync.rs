//! Locking and joining policy for the CLI and the hosts it shares with the tests.

use std::panic::resume_unwind;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use tokio::task::JoinError;

/// Lock `mutex`, recovering the guard if a previous holder panicked. Every
/// production mutex in the CLI and this crate goes through here: propagating poison would
/// kill the `EpochTicker` and `serve`'s epoch thread, and with them timeout
/// enforcement.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a task returned. A panic in it continues in the caller, so it ends
/// the run as it would have had the work run there; nothing cancels a task
/// whose result is joined here.
pub fn joined<T>(task: Result<T, JoinError>) -> T {
    task.unwrap_or_else(|e| resume_unwind(e.into_panic()))
}

/// Join `thread` from a `Drop`. Its panic continues here, unless the drop is
/// itself part of an unwind, where a second panic would abort the process.
pub fn join_in_drop(thread: JoinHandle<()>) {
    if let Err(panic) = thread.join()
        && !std::thread::panicking()
    {
        resume_unwind(panic);
    }
}
