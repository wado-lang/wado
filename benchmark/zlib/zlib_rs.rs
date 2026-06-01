// zlib-rs native benchmark for comparison with Wado's core:zlib
//
// Compresses and decompresses twitter.json (~631KB), reporting compression and
// decompression throughput (MB/s of the original data). Each phase
// auto-calibrates its iteration count to run for about a second.

use std::time::Instant;
use zlib_rs::{DeflateConfig, InflateConfig, ReturnCode};

const TARGET_NS: u128 = 1_000_000_000; // ~1s budget per phase

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

fn report(label: &str, work_per_iter: f64, n: u64, elapsed_ns: u128, unit: &str) {
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
    println!("{label}: {rbuf}   ({per_ms:.3} ms/iter, {n} iter)");
}

// Calibrate `f` to run for about `TARGET_NS`, then report its throughput.
fn bench<T, F: FnMut() -> T>(label: &str, work_per_iter: f64, unit: &str, mut f: F) -> T {
    let mut result = f(); // warmup
    let mut iters: u64 = 1;
    let elapsed: u128;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            result = f();
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
    report(label, work_per_iter, iters, elapsed, unit);
    result
}

fn main() {
    let data = std::fs::read("json_twitter/twitter.json")
        .expect("failed to read json_twitter/twitter.json");
    let size = data.len();
    let bound = zlib_rs::compress_bound(size);

    println!("zlib {size} bytes");

    let compressed = bench("Compress", size as f64, "B", || {
        let mut buf = vec![0u8; bound];
        let (result, rc) = zlib_rs::compress_slice(&mut buf, &data, DeflateConfig::new(6));
        assert_eq!(rc, ReturnCode::Ok);
        result.to_vec()
    });

    println!("Compressed: {} -> {} bytes", data.len(), compressed.len());

    let decompressed = bench("Decompress", size as f64, "B", || {
        let mut buf = vec![0u8; size];
        let (result, rc) =
            zlib_rs::decompress_slice(&mut buf, &compressed, InflateConfig::default());
        assert_eq!(rc, ReturnCode::Ok);
        result.to_vec()
    });

    // Verify round-trip.
    assert_eq!(decompressed.len(), size, "decompressed size mismatch");
    assert_eq!(decompressed, data.as_slice(), "decompressed data mismatch");
}
