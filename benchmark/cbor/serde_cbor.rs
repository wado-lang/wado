#![allow(dead_code)]
// Rust serde_cbor benchmark for twitter.json.
//
// Comparison baseline for Wado's core:cbor serialization/deserialization. The
// complete twitter.json schema is modeled with typed structs (no dynamic
// value), so every field is round-tripped. Throughput is reported over the
// original JSON source size (shared denominator across implementations). Both
// phases operate on byte buffers:
//   ser: TwitterResponse -> CBOR bytes  (serde_cbor::to_vec)
//   de:  CBOR bytes -> TwitterResponse  (serde_cbor::from_slice)
//
// serde matches struct field names to the wire keys by default, so no `rename`
// attributes are needed; `r#type` covers the one reserved-word key.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Serialize, Deserialize)]
struct SearchMetadata {
    completed_in: f64,
    count: i32,
    max_id: i64,
    max_id_str: String,
    next_results: String,
    query: String,
    refresh_url: String,
    since_id: i64,
    since_id_str: String,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    iso_language_code: String,
    result_type: String,
}

#[derive(Serialize, Deserialize)]
struct Hashtag {
    text: String,
    indices: Vec<i32>,
}

#[derive(Serialize, Deserialize)]
struct Url {
    url: String,
    expanded_url: String,
    display_url: String,
    indices: Vec<i32>,
}

#[derive(Serialize, Deserialize)]
struct UserMention {
    screen_name: String,
    name: String,
    id: i64,
    id_str: String,
    indices: Vec<i32>,
}

#[derive(Serialize, Deserialize)]
struct Size {
    w: i32,
    h: i32,
    resize: String,
}

#[derive(Serialize, Deserialize)]
struct MediaSizes {
    large: Size,
    medium: Size,
    small: Size,
    thumb: Size,
}

#[derive(Serialize, Deserialize)]
struct Media {
    id: i64,
    id_str: String,
    indices: Vec<i32>,
    media_url: String,
    media_url_https: String,
    url: String,
    display_url: String,
    expanded_url: String,
    r#type: String,
    sizes: MediaSizes,
    #[serde(default)]
    source_status_id: Option<i64>,
    #[serde(default)]
    source_status_id_str: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StatusEntities {
    hashtags: Vec<Hashtag>,
    symbols: Vec<String>,
    urls: Vec<Url>,
    user_mentions: Vec<UserMention>,
    #[serde(default)]
    media: Vec<Media>,
}

#[derive(Serialize, Deserialize)]
struct UserEntityUrls {
    urls: Vec<Url>,
}

#[derive(Serialize, Deserialize)]
struct UserEntities {
    description: UserEntityUrls,
    #[serde(default)]
    url: Option<UserEntityUrls>,
}

#[derive(Serialize, Deserialize)]
struct User {
    id: i64,
    id_str: String,
    name: String,
    screen_name: String,
    location: String,
    description: String,
    url: Option<String>,
    entities: UserEntities,
    protected: bool,
    followers_count: i32,
    friends_count: i32,
    listed_count: i32,
    created_at: String,
    favourites_count: i32,
    utc_offset: Option<i32>,
    time_zone: Option<String>,
    geo_enabled: bool,
    verified: bool,
    statuses_count: i32,
    lang: String,
    contributors_enabled: bool,
    is_translator: bool,
    is_translation_enabled: bool,
    profile_background_color: String,
    profile_background_image_url: String,
    profile_background_image_url_https: String,
    profile_background_tile: bool,
    profile_image_url: String,
    profile_image_url_https: String,
    #[serde(default)]
    profile_banner_url: Option<String>,
    profile_link_color: String,
    profile_sidebar_border_color: String,
    profile_sidebar_fill_color: String,
    profile_text_color: String,
    profile_use_background_image: bool,
    default_profile: bool,
    default_profile_image: bool,
    following: bool,
    follow_request_sent: bool,
    notifications: bool,
}

#[derive(Serialize, Deserialize)]
struct Status {
    metadata: Metadata,
    created_at: String,
    id: i64,
    id_str: String,
    text: String,
    source: String,
    truncated: bool,
    in_reply_to_status_id: Option<i64>,
    in_reply_to_status_id_str: Option<String>,
    in_reply_to_user_id: Option<i64>,
    in_reply_to_user_id_str: Option<String>,
    in_reply_to_screen_name: Option<String>,
    user: User,
    geo: Option<String>,
    coordinates: Option<String>,
    place: Option<String>,
    contributors: Option<String>,
    #[serde(default)]
    retweeted_status: Option<Box<Status>>,
    retweet_count: i32,
    favorite_count: i32,
    entities: StatusEntities,
    favorited: bool,
    retweeted: bool,
    #[serde(default)]
    possibly_sensitive: bool,
    lang: String,
}

#[derive(Serialize, Deserialize)]
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
    let json_data = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../json_twitter/twitter.json"
    ))
    .expect("Failed to read twitter.json");
    let json_size = json_data.len();

    let resp: TwitterResponse =
        serde_json::from_str(&json_data).expect("Failed to parse twitter.json");
    let cbor = serde_cbor::to_vec(&resp).expect("Failed to encode CBOR");

    println!(
        "cbor-twitter: {json_size} bytes JSON -> {} bytes CBOR payload",
        cbor.len()
    );

    bench("Ser", json_size as f64, "B", || {
        serde_cbor::to_vec(&resp).expect("encode").len()
    });

    let count = bench("De", json_size as f64, "B", || {
        let r: TwitterResponse = serde_cbor::from_slice(&cbor).expect("decode");
        r.statuses.len()
    });

    assert_eq!(count, 100);
    println!("Round-tripped {count} statuses per iteration");
}
