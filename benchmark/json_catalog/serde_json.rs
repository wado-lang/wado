#![allow(dead_code)]
// Rust serde_json benchmark for citm_catalog.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    description: Option<String>,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    name: String,
    #[serde(rename = "subTopicIds")]
    sub_topic_ids: Vec<i64>,
    #[serde(default, rename = "subjectCode")]
    subject_code: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(rename = "topicIds")]
    topic_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct Price {
    amount: i64,
    #[serde(rename = "audienceSubCategoryId")]
    audience_sub_category_id: i64,
    #[serde(rename = "seatCategoryId")]
    seat_category_id: i64,
}

#[derive(Deserialize)]
struct Area {
    #[serde(rename = "areaId")]
    area_id: i64,
    #[serde(rename = "blockIds")]
    block_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct SeatCategory {
    areas: Vec<Area>,
    #[serde(rename = "seatCategoryId")]
    seat_category_id: i64,
}

#[derive(Deserialize)]
struct Performance {
    #[serde(rename = "eventId")]
    event_id: i64,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    prices: Vec<Price>,
    #[serde(rename = "seatCategories")]
    seat_categories: Vec<SeatCategory>,
    #[serde(default, rename = "seatMapImage")]
    seat_map_image: Option<String>,
    start: i64,
    #[serde(rename = "venueCode")]
    venue_code: String,
}

#[derive(Deserialize)]
struct CitmCatalog {
    #[serde(rename = "areaNames")]
    area_names: BTreeMap<String, String>,
    #[serde(rename = "audienceSubCategoryNames")]
    audience_sub_category_names: BTreeMap<String, String>,
    #[serde(rename = "blockNames")]
    block_names: BTreeMap<String, String>,
    events: BTreeMap<String, Event>,
    performances: Vec<Performance>,
    #[serde(rename = "seatCategoryNames")]
    seat_category_names: BTreeMap<String, String>,
    #[serde(rename = "subTopicNames")]
    sub_topic_names: BTreeMap<String, String>,
    #[serde(rename = "subjectNames")]
    subject_names: BTreeMap<String, String>,
    #[serde(rename = "topicNames")]
    topic_names: BTreeMap<String, String>,
    #[serde(rename = "topicSubTopics")]
    topic_sub_topics: BTreeMap<String, Vec<i64>>,
    #[serde(rename = "venueNames")]
    venue_names: BTreeMap<String, String>,
}

fn main() {
    let json_data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/citm_catalog.json"))
        .expect("Failed to read citm_catalog.json");
    let iterations = 10;

    println!(
        "json-catalog: {} bytes, {} iterations",
        json_data.len(),
        iterations
    );

    let start = Instant::now();
    let mut total_events = 0usize;
    let mut total_performances = 0usize;
    for _ in 0..iterations {
        let catalog: CitmCatalog =
            serde_json::from_str(&json_data).expect("Failed to parse citm_catalog.json");
        total_events += catalog.events.len();
        total_performances += catalog.performances.len();
    }
    let elapsed = start.elapsed();

    assert_eq!(total_events, 184 * iterations);
    assert_eq!(total_performances, 243 * iterations);
    println!(
        "Parsed {} events, {} performances",
        total_events, total_performances
    );
    println!(
        "Elapsed: {}.{:03} ms",
        elapsed.as_millis(),
        elapsed.as_micros() % 1000
    );
}
