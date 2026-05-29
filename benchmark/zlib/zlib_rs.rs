// zlib-rs native benchmark for comparison with Wado's core:zlib
//
// Compresses and decompresses twitter.json (~631KB) x10 iterations.

use std::time::Instant;
use zlib_rs::{DeflateConfig, InflateConfig, ReturnCode};

fn main() {
    let data = std::fs::read("json_twitter/twitter.json")
        .expect("failed to read json_twitter/twitter.json");
    let size = data.len();
    let iterations = 100;

    println!("zlib {size} bytes x {iterations} iterations");

    // Benchmark compression (10 iterations)
    let bound = zlib_rs::compress_bound(size);
    let mut compressed = Vec::new();

    let t0 = Instant::now();
    for _ in 0..iterations {
        let mut buf = vec![0u8; bound];
        let (result, rc) = zlib_rs::compress_slice(&mut buf, &data, DeflateConfig::new(6));
        assert_eq!(rc, ReturnCode::Ok);
        compressed = result.to_vec();
    }
    let compress_time = t0.elapsed();

    println!(
        "Compressed: {} -> {} bytes",
        data.len(),
        compressed.len()
    );
    println!(
        "Compress: {}.{:03} ms",
        compress_time.as_millis(),
        compress_time.as_micros() % 1000
    );

    // Benchmark decompression (10 iterations)
    let t1 = Instant::now();
    let mut decompressed = Vec::new();
    for _ in 0..iterations {
        let mut buf = vec![0u8; size];
        let (result, rc) =
            zlib_rs::decompress_slice(&mut buf, &compressed, InflateConfig::default());
        assert_eq!(rc, ReturnCode::Ok);
        decompressed = result.to_vec();
    }
    let decompress_time = t1.elapsed();

    println!(
        "Decompress: {}.{:03} ms",
        decompress_time.as_millis(),
        decompress_time.as_micros() % 1000
    );

    // Verify round-trip
    assert_eq!(decompressed.len(), size, "decompressed size mismatch");
    assert_eq!(decompressed, data.as_slice(), "decompressed data mismatch");

    let total = compress_time + decompress_time;
    println!(
        "Elapsed: {}.{:03} ms",
        total.as_millis(),
        total.as_micros() % 1000
    );
}
