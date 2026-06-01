#![allow(dead_code)]
// Rust serde_json benchmark for canada.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde::Deserialize;
use std::time::Instant;

#[derive(Deserialize)]
struct Properties {
    name: String,
}

#[derive(Deserialize)]
struct Geometry {
    #[serde(rename = "type")]
    geom_type: String,
    coordinates: Vec<Vec<Vec<f64>>>,
}

#[derive(Deserialize)]
struct Feature {
    #[serde(rename = "type")]
    feat_type: String,
    properties: Properties,
    geometry: Geometry,
}

#[derive(Deserialize)]
struct FeatureCollection {
    #[serde(rename = "type")]
    collection_type: String,
    features: Vec<Feature>,
}

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

fn main() {
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/canada.json"))
        .expect("Failed to read canada.json");
    let size = json_data.len();

    println!("json-canada: {size} bytes");

    let total_points = bench("Throughput", size as f64, "B", || {
        let fc: FeatureCollection =
            serde_json::from_str(&json_data).expect("Failed to parse canada.json");
        let mut points = 0usize;
        for feat in &fc.features {
            for ring in &feat.geometry.coordinates {
                points += ring.len();
            }
        }
        points
    });

    assert_eq!(total_points, 55563);
    println!("Parsed {total_points} coordinate points per iteration");
}
