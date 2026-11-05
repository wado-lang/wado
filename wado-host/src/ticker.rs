//! A thread on an interval, stopped by dropping it.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::sync::{join_in_drop, lock};

/// An OS thread calling `tick` every `interval` until the ticker drops.
/// Dropping wakes the thread, so it never waits out the interval in flight,
/// and no tick runs once the drop returns.
pub struct Ticker {
    control: Arc<(Mutex<TickerControl>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct TickerControl {
    stopped: bool,
    restarted: bool,
}

impl Ticker {
    pub fn start(interval: Duration, mut tick: impl FnMut() + Send + 'static) -> Self {
        let control = Arc::new((Mutex::new(TickerControl::default()), Condvar::new()));
        let handle = std::thread::spawn({
            let control = Arc::clone(&control);
            move || {
                let (state, wake) = &*control;
                loop {
                    let (mut held, wait) = wake
                        .wait_timeout_while(lock(state), interval, |state| {
                            !state.stopped && !state.restarted
                        })
                        .unwrap_or_else(PoisonError::into_inner);
                    if held.stopped {
                        return;
                    }
                    held.restarted = false;
                    // A wait of no interval never lets the lock go, and a drop
                    // needs it to stop this thread.
                    drop(held);
                    if wait.timed_out() {
                        tick();
                    }
                }
            }
        });
        Self {
            control,
            handle: Some(handle),
        }
    }

    /// Start the interval in flight over, so the next tick is a whole
    /// interval away.
    pub fn restart(&self) {
        let (state, wake) = &*self.control;
        lock(state).restarted = true;
        wake.notify_all();
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        let (state, wake) = &*self.control;
        lock(state).stopped = true;
        wake.notify_all();
        if let Some(handle) = self.handle.take() {
            join_in_drop(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn dropping_a_ticker_does_not_wait_out_its_interval() {
        let ticker = Ticker::start(Duration::from_secs(60), || {});
        // Dropped before its thread first runs, a ticker stops at the loop
        // head and the wait is never exercised.
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        drop(ticker);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_ticker_with_no_interval_ticks_and_still_drops() {
        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ticker = Ticker::start(Duration::ZERO, {
            let ticks = Arc::clone(&ticks);
            move || {
                ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(ticker);
        assert!(ticks.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn a_restart_holds_off_the_next_tick() {
        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ticker = Ticker::start(Duration::from_millis(300), {
            let ticks = Arc::clone(&ticks);
            move || {
                ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(100));
            ticker.restart();
        }
        assert_eq!(ticks.load(std::sync::atomic::Ordering::Relaxed), 0);
        std::thread::sleep(Duration::from_millis(600));
        assert!(ticks.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }
}
