//! The threads the optimizer visits functions on. The host picks how many
//! (`CompilerOptions::parallelism`); the compiler never does. One thread, or a
//! `wasm32` build, runs the same visits in order, and either way a result list
//! comes back in item order, so the output never depends on the thread count
//! (WEP: Parallel Optimizer).

use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;

/// Lock a memo the visits share. A visit that panics fails the whole compile,
/// so nothing reads what a poisoned lock holds.
pub fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs a visit per item on a pool of `threads`, or in order without one.
pub struct Executor {
    #[cfg(not(target_arch = "wasm32"))]
    pool: Option<rayon::ThreadPool>,
}

impl Executor {
    /// An executor running on `threads` threads; one or fewer runs in order.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let pool = (threads > 1).then(|| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .thread_name(|i| format!("wado-opt-{i}"))
                    .build()
                    .expect("the optimizer's thread pool starts")
            });
            Self { pool }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = threads;
            Self {}
        }
    }

    /// `f` of each item, in item order.
    pub fn map<T: Sync, R: Send>(&self, items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            return pool.install(|| items.par_iter().map(f).collect());
        }
        items.iter().map(f).collect()
    }

    /// [`Self::map`], each thread handing `f` one scratch state made by
    /// `init` and reused across the items it visits. What `f` returns must not
    /// depend on what an earlier item left in the state.
    pub fn map_init<T: Sync, S, R: Send>(
        &self,
        items: &[T],
        init: impl Fn() -> S + Sync + Send,
        f: impl Fn(&mut S, &T) -> R + Sync + Send,
    ) -> Vec<R> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            return pool.install(|| items.par_iter().map_init(init, f).collect());
        }
        let mut state = init();
        items.iter().map(|item| f(&mut state, item)).collect()
    }

    /// `f` of each index below `len`, in index order.
    pub fn map_indices<R: Send>(&self, len: usize, f: impl Fn(usize) -> R + Sync + Send) -> Vec<R> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = &self.pool {
            return pool.install(|| (0..len).into_par_iter().map(f).collect());
        }
        (0..len).map(f).collect()
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new(1)
    }
}
