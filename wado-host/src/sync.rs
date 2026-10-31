//! Locking and joining policy for the CLI and the hosts it shares with the tests.

use std::io;
use std::panic::resume_unwind;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use tokio::task::{JoinError, JoinSet};

/// Lock `mutex`, recovering the guard if a previous holder panicked. Every
/// production mutex in the CLI and this crate goes through here: propagating poison would
/// kill the `EpochTicker` and `serve`'s epoch thread, and with them timeout
/// enforcement.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wait on `condvar` while `condition` holds, recovering the guard as [`lock`]
/// does.
pub fn wait_while<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    condition: impl FnMut(&mut T) -> bool,
) -> MutexGuard<'a, T> {
    condvar
        .wait_while(guard, condition)
        .unwrap_or_else(PoisonError::into_inner)
}

/// What a task returned. A panic in it continues in the caller, so it ends
/// the run as it would have had the work run there; nothing cancels a task
/// whose result is joined here.
pub fn joined<T>(task: Result<T, JoinError>) -> T {
    task.unwrap_or_else(|e| resume_unwind(e.into_panic()))
}

/// What the next of `tasks` to end returned, as [`joined`] reads it. An empty
/// set never answers, so a `select` over this waits on its other side.
pub async fn next_joined<T: 'static>(tasks: &mut JoinSet<T>) -> T {
    match tasks.join_next().await {
        Some(task) => joined(task),
        None => std::future::pending().await,
    }
}

/// What `thread` returned. Its panic continues in the caller.
pub fn join<T>(thread: JoinHandle<T>) -> T {
    thread.join().unwrap_or_else(|panic| resume_unwind(panic))
}

/// Start `body` on a thread nobody joins, for one that may outlive whoever
/// would. Its panic ends the process, as a joined thread's ends the run.
pub fn spawn_detached(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    struct AbortOnPanic;
    impl Drop for AbortOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                std::process::abort();
            }
        }
    }
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _abort = AbortOnPanic;
            body();
        })
        .map(drop)
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
