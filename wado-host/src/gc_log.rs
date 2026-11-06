//! `--gc-log`: a line on stderr for each collection a guest's store runs.
//!
//! Wasmtime has no API that reports a collection, only the text of its trace
//! log, so this installs a logger and reads that text. The workspace pins
//! wasmtime, and a change to the text panics in `--gc-log`'s integration test.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::sync::lock;

/// The module whose trace holds `do_gc`'s two reports.
const GC_TARGET: &str = "wasmtime::runtime::store::gc";
const BEGIN_GC: &str = "============ Begin GC";
const END_GC: &str = "============ End GC";

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

struct Tally {
    run_started: Instant,
    collection_started: Option<Instant>,
    collections: u64,
    paused: Duration,
}

static TALLY: Mutex<Option<Tally>> = Mutex::new(None);

struct GcLogger;

/// Starts logging collections. The run must be the only thing in the process
/// collecting: the log names no store, so a line pairs a collection's end with
/// the last one that began.
///
/// # Panics
///
/// Panics if a logger is already installed.
pub fn start() {
    *lock(&TALLY) = Some(Tally {
        run_started: Instant::now(),
        collection_started: None,
        collections: 0,
        paused: Duration::ZERO,
    });
    log::set_logger(&GcLogger).expect("--gc-log installs the process's only logger");
    log::set_max_level(LevelFilter::Trace);
}

/// Writes the total of what [`start`] logged.
pub fn finish() {
    let tally = lock(&TALLY);
    let tally = tally.as_ref().expect("--gc-log started the log");
    eprintln!(
        "gc total: {} collections, paused {:.1} ms",
        tally.collections,
        millis(tally.paused)
    );
}

impl Log for GcLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() == Level::Trace && metadata.target() == GC_TARGET
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = Instant::now();
        let text = record.args().to_string();
        let mut tally = lock(&TALLY);
        let tally = tally.as_mut().expect("--gc-log started the log");
        if text.starts_with(BEGIN_GC) {
            tally.collection_started = Some(now);
        } else if text.starts_with(END_GC) {
            let started = tally
                .collection_started
                .take()
                .expect("a collection ends after it begins");
            let paused = now - started;
            tally.collections += 1;
            tally.paused += paused;
            let (live, capacity) = end_of_gc_sizes(&text);
            eprintln!(
                "gc {} at {:.1} ms: paused {:.2} ms, live {:.1} MiB of {:.1} MiB",
                tally.collections,
                millis(started - tally.run_started),
                millis(paused),
                mib(live),
                mib(capacity)
            );
        }
    }

    fn flush(&self) {}
}

/// The live set and capacity in bytes that an end-of-collection report states.
fn end_of_gc_sizes(report: &str) -> (u64, u64) {
    (
        reported_size(report, "post-GC live-set size"),
        reported_size(report, "GC heap capacity"),
    )
}

fn reported_size(report: &str, label: &str) -> u64 {
    let line = report
        .lines()
        .find(|line| line.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("wasmtime's GC report must state `{label}`:\n{report}"));
    let hex = line
        .split_once("= 0x")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("wasmtime's GC report must state a hex size:\n{line}"));
    u64::from_str_radix(hex, 16).expect("wasmtime prints a size in hex")
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a size is printed to a tenth of a MiB"
)]
fn mib(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_MIB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_sizes_of_an_end_of_collection_report() {
        let report = "============ End GC ===========\n\
                      \t     GC heap capacity = 0x10000000 bytes\n\
                      \tpost-GC live-set size = 0x00768330 bytes\n\
                      \t  GC heap utilization = 2.89%";
        assert_eq!(end_of_gc_sizes(report), (0x0076_8330, 0x1000_0000));
    }
}
