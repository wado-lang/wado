// Float-to-string benchmark
// Converts 1M random f64 values to decimal strings using {:.6} format.
// Uses a linear congruential generator for deterministic float sequence.
//
// Reports throughput (conversions per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-fts

use std::fmt::Write;
use std::time::Instant;

const TARGET_NS: u128 = 1_000_000_000; // ~1s budget

fn next_iters(n: u64, elapsed_ns: u128, target_ns: u128) -> u64 {
    let e = if elapsed_ns == 0 { 1 } else { elapsed_ns };
    let mut est = (n as u128) * target_ns / e;
    let hi = n as u128 * 100;
    if est > hi {
        est = hi;
    }
    if est > 1_000_000_000 {
        est = 1_000_000_000;
    }
    if est < 1 {
        est = 1;
    }
    est as u64
}

fn print_throughput(work_per_iter: f64, n: u64, elapsed_ns: u128, unit: &str) {
    let secs = elapsed_ns as f64 / 1e9;
    let rate = if secs > 0.0 {
        work_per_iter * n as f64 / secs
    } else {
        0.0
    };
    let per_ms = elapsed_ns as f64 / n as f64 / 1e6;
    let rbuf = if unit == "B" {
        if rate >= 1e9 {
            format!("{:.2} GB/s", rate / 1e9)
        } else if rate >= 1e6 {
            format!("{:.2} MB/s", rate / 1e6)
        } else if rate >= 1e3 {
            format!("{:.2} KB/s", rate / 1e3)
        } else {
            format!("{rate:.2} B/s")
        }
    } else if rate >= 1e9 {
        format!("{:.2} G {unit}/s", rate / 1e9)
    } else if rate >= 1e6 {
        format!("{:.2} M {unit}/s", rate / 1e6)
    } else if rate >= 1e3 {
        format!("{:.2} k {unit}/s", rate / 1e3)
    } else {
        format!("{rate:.2} {unit}/s")
    };
    println!("Throughput: {rbuf}   ({per_ms:.3} ms/iter, {n} iter)");
}

// Convert `n` LCG-derived f64 values to "{:.6}" strings, returning
// (total_bytes, byte_sum) so the conversions are not optimized away.
fn fts_run(n: u32) -> (u64, u64) {
    let mut state: u32 = 42;
    let mut total_bytes: u64 = 0;
    let mut byte_sum: u64 = 0;
    let mut buf = String::with_capacity(16);

    for _ in 0..n {
        state = (state.wrapping_mul(1103515245).wrapping_add(12345)) & 0x7FFFFFFF;
        let x = state as f64 / 2147483648.0;
        buf.clear();
        write!(buf, "{:.6}", x).unwrap();
        total_bytes += buf.len() as u64;
        for b in buf.bytes() {
            byte_sum += b as u64;
        }
    }

    (total_bytes, byte_sum)
}

fn main() {
    let n = 1_000_000u32;

    // Warmup.
    let (mut total_bytes, mut byte_sum) = fts_run(n);

    let mut iters: u64 = 1;
    let elapsed: u128;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            let r = fts_run(n);
            total_bytes = r.0;
            byte_sum = r.1;
        }
        let e = start.elapsed().as_nanos();
        if e >= TARGET_NS {
            elapsed = e;
            break;
        }
        let nx = next_iters(iters, e, TARGET_NS);
        if nx <= iters {
            elapsed = e;
            break;
        }
        iters = nx;
    }

    println!("fts: {n} f64 conversions (%.6f)");
    println!("Total bytes: {total_bytes}, byte sum: {byte_sum}");
    print_throughput(n as f64, iters, elapsed, "conversions");
}
