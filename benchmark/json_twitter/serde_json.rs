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

fn main() {
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/twitter.json"))
        .expect("Failed to read twitter.json");
    let iterations = 10;

    println!(
        "json-twitter: {} bytes, {} iterations",
        json_data.len(),
        iterations
    );

    let start = Instant::now();
    let mut count = 0usize;
    for _ in 0..iterations {
        let resp: TwitterResponse =
            serde_json::from_str(&json_data).expect("Failed to parse twitter.json");
        count += resp.statuses.len();
    }
    let elapsed = start.elapsed();

    assert_eq!(count, 100 * iterations);
    println!("Parsed {} total statuses", count);
    println!(
        "Elapsed: {}.{:03} ms",
        elapsed.as_millis(),
        elapsed.as_micros() % 1000
    );
}
