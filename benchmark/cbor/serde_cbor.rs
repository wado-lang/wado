#![allow(dead_code)]
// Rust serde_cbor benchmark for twitter.json.
//
// Comparison baseline for Wado's core:cbor serialization/deserialization.
// To keep the implementations comparable, the whole document is handled via a
// dynamic value (`serde_cbor::Value`, parsed from JSON with serde_json), and
// throughput is reported over the original JSON source size. Both phases operate
// on byte buffers:
//   ser: Value      -> CBOR bytes  (serde_cbor::to_vec)
//   de:  CBOR bytes -> Value       (serde_cbor::from_slice)
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde_cbor::Value;
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

fn statuses_count(v: &Value) -> usize {
    if let Value::Map(m) = v {
        if let Some(Value::Array(a)) = m.get(&Value::Text("statuses".to_string())) {
            return a.len();
        }
    }
    0
}

fn main() {
    let json_data = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../json_twitter/twitter.json"
    ))
    .expect("Failed to read twitter.json");
    let json_size = json_data.len();

    let value: Value = serde_json::from_str(&json_data).expect("Failed to parse twitter.json");
    let cbor = serde_cbor::to_vec(&value).expect("Failed to encode CBOR");

    println!(
        "cbor-twitter: {json_size} bytes JSON -> {} bytes CBOR payload",
        cbor.len()
    );

    bench("Ser", json_size as f64, "B", || {
        serde_cbor::to_vec(&value).expect("encode").len()
    });

    let count = bench("De", json_size as f64, "B", || {
        let v: Value = serde_cbor::from_slice(&cbor).expect("decode");
        statuses_count(&v)
    });

    assert_eq!(count, 100);
    println!("Round-tripped {count} statuses per iteration");
}
