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

fn main() {
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/canada.json"))
        .expect("Failed to read canada.json");
    let iterations = 10;

    println!(
        "json-canada: {} bytes, {} iterations",
        json_data.len(),
        iterations
    );

    let start = Instant::now();
    let mut total_points = 0usize;
    for _ in 0..iterations {
        let fc: FeatureCollection =
            serde_json::from_str(&json_data).expect("Failed to parse canada.json");
        for feat in &fc.features {
            for ring in &feat.geometry.coordinates {
                total_points += ring.len();
            }
        }
    }
    let elapsed = start.elapsed();

    assert_eq!(total_points, 55563 * iterations);
    println!("Parsed {} total coordinate points", total_points);
    println!(
        "Elapsed: {}.{:03} ms",
        elapsed.as_millis(),
        elapsed.as_micros() % 1000
    );
}
