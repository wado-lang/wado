// Float-to-string benchmark
// Converts 5M random f64 values to decimal strings using {:.6} format.
// Uses a linear congruential generator for deterministic float sequence.
//
// How to run:
//   mise run benchmark-fts

use std::fmt::Write;
use std::time::Instant;

fn main() {
    let n = 5_000_000u32;
    let mut state: u32 = 42;
    let mut total_bytes: u64 = 0;
    let mut byte_sum: u64 = 0;
    let mut buf = String::with_capacity(16);

    let t0 = Instant::now();

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

    let elapsed_ms = t0.elapsed().as_millis();

    println!("fts: {n} f64 conversions (%.6f)");
    println!("Total bytes: {total_bytes}, byte sum: {byte_sum}");
    println!("Elapsed: {elapsed_ms} ms");
}
