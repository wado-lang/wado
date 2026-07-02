#![allow(dead_code)]
// Rust serde_json benchmark for citm_catalog.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    #[serde(default)]
    description: Option<String>,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    name: String,
    sub_topic_ids: Vec<i64>,
    #[serde(default)]
    subject_code: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    topic_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Price {
    amount: i64,
    audience_sub_category_id: i64,
    seat_category_id: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Area {
    area_id: i64,
    block_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeatCategory {
    areas: Vec<Area>,
    seat_category_id: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Performance {
    event_id: i64,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    prices: Vec<Price>,
    seat_categories: Vec<SeatCategory>,
    #[serde(default)]
    seat_map_image: Option<String>,
    start: i64,
    venue_code: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CitmCatalog {
    area_names: BTreeMap<String, String>,
    audience_sub_category_names: BTreeMap<String, String>,
    block_names: BTreeMap<String, String>,
    events: BTreeMap<String, Event>,
    performances: Vec<Performance>,
    seat_category_names: BTreeMap<String, String>,
    sub_topic_names: BTreeMap<String, String>,
    subject_names: BTreeMap<String, String>,
    topic_names: BTreeMap<String, String>,
    topic_sub_topics: BTreeMap<String, Vec<i64>>,
    venue_names: BTreeMap<String, String>,
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
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/citm_catalog.json"))
        .expect("Failed to read citm_catalog.json");
    let size = json_data.len();

    let catalog: CitmCatalog =
        serde_json::from_str(&json_data).expect("Failed to parse citm_catalog.json");
    assert_eq!(catalog.performances.len(), 243);

    println!("json-catalog: {size} bytes");

    bench("Ser", size as f64, "B", || {
        serde_json::to_vec(&catalog).expect("encode").len()
    });

    let events = bench("De", size as f64, "B", || {
        let catalog: CitmCatalog = serde_json::from_str(&json_data).expect("decode");
        catalog.events.len()
    });

    assert_eq!(events, 184);
    println!("Round-tripped {events} events per iteration");
}
