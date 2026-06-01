#![allow(dead_code)]
// Rust serde_json benchmark for twitter.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde::Deserialize;
use std::time::Instant;

#[derive(Deserialize)]
struct SearchMetadata {
    completed_in: f64,
    count: i32,
    max_id: i64,
    max_id_str: String,
    query: String,
    since_id: i64,
    since_id_str: String,
}

#[derive(Deserialize)]
struct Metadata {
    iso_language_code: String,
    result_type: String,
}

#[derive(Deserialize)]
struct Url {
    url: String,
    expanded_url: String,
    display_url: String,
    indices: Vec<i32>,
}

#[derive(Deserialize)]
struct Hashtag {
    text: String,
    indices: Vec<i32>,
}

#[derive(Deserialize)]
struct UserMention {
    screen_name: String,
    name: String,
    id: i64,
    id_str: String,
    indices: Vec<i32>,
}

#[derive(Deserialize)]
struct StatusEntities {
    hashtags: Vec<Hashtag>,
    urls: Vec<Url>,
    user_mentions: Vec<UserMention>,
}

#[derive(Deserialize)]
struct UserEntities {
    #[serde(default)]
    description: Option<UserEntityUrls>,
    #[serde(default)]
    url: Option<UserEntityUrls>,
}

#[derive(Deserialize)]
struct UserEntityUrls {
    #[serde(default)]
    urls: Vec<Url>,
}

#[derive(Deserialize)]
struct User {
    id: i64,
    id_str: String,
    name: String,
    screen_name: String,
    #[serde(default)]
    description: Option<String>,
    followers_count: i32,
    friends_count: i32,
    statuses_count: i32,
    created_at: String,
    profile_image_url_https: String,
    #[serde(default)]
    verified: bool,
    lang: String,
}

#[derive(Deserialize)]
struct Status {
    metadata: Metadata,
    created_at: String,
    id: i64,
    id_str: String,
    text: String,
    source: String,
    truncated: bool,
    user: User,
    retweet_count: i32,
    favorite_count: i32,
    entities: StatusEntities,
    favorited: bool,
    retweeted: bool,
    lang: String,
}

#[derive(Deserialize)]
struct TwitterResponse {
    statuses: Vec<Status>,
    search_metadata: SearchMetadata,
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
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/twitter.json"))
        .expect("Failed to read twitter.json");
    let size = json_data.len();

    println!("json-twitter: {size} bytes");

    let count = bench("Throughput", size as f64, "B", || {
        let resp: TwitterResponse =
            serde_json::from_str(&json_data).expect("Failed to parse twitter.json");
        resp.statuses.len()
    });

    assert_eq!(count, 100);
    println!("Parsed {count} statuses per iteration");
}
