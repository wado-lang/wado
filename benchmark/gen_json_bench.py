#!/usr/bin/env python3
"""Generate Wado benchmark source files with embedded JSON data.

JSON test data from: https://github.com/miloyip/nativejson-benchmark/tree/master/data
License: MIT (https://github.com/miloyip/nativejson-benchmark/blob/master/license.txt)

Usage:
    python3 gen_json_bench.py
"""
import os

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))


def escape_wado_string(s: str) -> str:
    """Escape a string for use in a Wado string literal."""
    return (
        s.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )


# --- twitter.json ---

TWITTER_STRUCTS = r'''
// JSON benchmark: twitter.json
// Data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT
//
// Parses a Twitter API search response with 100 statuses.

use { println, Stdout } from "core:cli";
use { Serialize, Deserialize } from "core:serde";
use { from_string } from "core:json";
use { MonotonicClock } from "wasi:clocks";

struct SearchMetadata {
    #[serde(rename = "completed_in")]
    completed_in: f64,
    count: i32,
    #[serde(rename = "max_id")]
    max_id: i64,
    #[serde(rename = "max_id_str")]
    max_id_str: String,
    query: String,
    #[serde(rename = "since_id")]
    since_id: i64,
    #[serde(rename = "since_id_str")]
    since_id_str: String,
}
impl Deserialize for SearchMetadata;

struct Metadata {
    #[serde(rename = "iso_language_code")]
    iso_language_code: String,
    #[serde(rename = "result_type")]
    result_type: String,
}
impl Deserialize for Metadata;

struct Url {
    url: String,
    #[serde(rename = "expanded_url")]
    expanded_url: String,
    #[serde(rename = "display_url")]
    display_url: String,
    indices: Array<i32>,
}
impl Deserialize for Url;

struct Hashtag {
    text: String,
    indices: Array<i32>,
}
impl Deserialize for Hashtag;

struct UserMention {
    #[serde(rename = "screen_name")]
    screen_name: String,
    name: String,
    id: i64,
    #[serde(rename = "id_str")]
    id_str: String,
    indices: Array<i32>,
}
impl Deserialize for UserMention;

struct StatusEntities {
    hashtags: Array<Hashtag>,
    urls: Array<Url>,
    #[serde(rename = "user_mentions")]
    user_mentions: Array<UserMention>,
}
impl Deserialize for StatusEntities;

struct UserEntities {
    #[serde(default)]
    description: UserEntityUrls,
    #[serde(default)]
    url: UserEntityUrls,
}
impl Deserialize for UserEntities;

struct UserEntityUrls {
    #[serde(default)]
    urls: Array<Url>,
}
impl Deserialize for UserEntityUrls;

struct User {
    id: i64,
    #[serde(rename = "id_str")]
    id_str: String,
    name: String,
    #[serde(rename = "screen_name")]
    screen_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "followers_count")]
    followers_count: i32,
    #[serde(rename = "friends_count")]
    friends_count: i32,
    #[serde(rename = "statuses_count")]
    statuses_count: i32,
    #[serde(rename = "created_at")]
    created_at: String,
    #[serde(rename = "profile_image_url_https")]
    profile_image_url_https: String,
    #[serde(default)]
    verified: bool,
    lang: String,
}
impl Deserialize for User;

struct Status {
    metadata: Metadata,
    #[serde(rename = "created_at")]
    created_at: String,
    id: i64,
    #[serde(rename = "id_str")]
    id_str: String,
    text: String,
    source: String,
    truncated: bool,
    user: User,
    #[serde(rename = "retweet_count")]
    retweet_count: i32,
    #[serde(rename = "favorite_count")]
    favorite_count: i32,
    entities: StatusEntities,
    favorited: bool,
    retweeted: bool,
    lang: String,
}
impl Deserialize for Status;

struct TwitterResponse {
    statuses: Array<Status>,
    #[serde(rename = "search_metadata")]
    search_metadata: SearchMetadata,
}
impl Deserialize for TwitterResponse;
'''

TWITTER_BENCH = r'''
export fn run() with Stdout, MonotonicClock {
    let iterations = 10;
    println(`json-twitter: {JSON_DATA.len()} bytes, {iterations} iterations`);

    let start = MonotonicClock::now();
    let mut count = 0;
    for let mut i = 0; i < iterations; i += 1 {
        let result = from_string::<TwitterResponse>(JSON_DATA);
        if let Ok(resp) = result {
            count = count + resp.statuses.len();
        } else {
            if let Err(e) = result {
                println(`Parse error: {e.message}`);
            }
            break;
        }
    }
    let end = MonotonicClock::now();
    let elapsed_ns = (end - start) as u64;
    let elapsed_ms = elapsed_ns / 1000000;
    let elapsed_us = elapsed_ns / 1000;

    assert count == 100 * iterations;
    println(`Parsed {count} total statuses`);
    println(`Elapsed: {elapsed_ms}.{elapsed_us % 1000} ms`);
}
'''


# --- canada.json ---

CANADA_STRUCTS = r'''
// JSON benchmark: canada.json
// Data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT
//
// Parses a GeoJSON FeatureCollection with 480 polygon rings and 55,563 coordinate points.

use { println, Stdout } from "core:cli";
use { Serialize, Deserialize } from "core:serde";
use { from_string } from "core:json";
use { MonotonicClock } from "wasi:clocks";

struct Properties {
    name: String,
}
impl Deserialize for Properties;

struct Geometry {
    #[serde(rename = "type")]
    geom_type: String,
    coordinates: Array<Array<Array<f64>>>,
}
impl Deserialize for Geometry;

struct Feature {
    #[serde(rename = "type")]
    feat_type: String,
    properties: Properties,
    geometry: Geometry,
}
impl Deserialize for Feature;

struct FeatureCollection {
    #[serde(rename = "type")]
    collection_type: String,
    features: Array<Feature>,
}
impl Deserialize for FeatureCollection;
'''

CANADA_BENCH = r'''
export fn run() with Stdout, MonotonicClock {
    let iterations = 10;
    println(`json-canada: {JSON_DATA.len()} bytes, {iterations} iterations`);

    let start = MonotonicClock::now();
    let mut total_points = 0;
    for let mut i = 0; i < iterations; i += 1 {
        let result = from_string::<FeatureCollection>(JSON_DATA);
        if let Ok(fc) = result {
            for let feat of fc.features {
                for let ring of feat.geometry.coordinates {
                    total_points = total_points + ring.len();
                }
            }
        } else {
            if let Err(e) = result {
                println(`Parse error: {e.message}`);
            }
            break;
        }
    }
    let end = MonotonicClock::now();
    let elapsed_ns = (end - start) as u64;
    let elapsed_ms = elapsed_ns / 1000000;
    let elapsed_us = elapsed_ns / 1000;

    assert total_points == 55563 * iterations;
    println(`Parsed {total_points} total coordinate points`);
    println(`Elapsed: {elapsed_ms}.{elapsed_us % 1000} ms`);
}
'''


# --- citm_catalog.json ---

CATALOG_STRUCTS = r'''
// JSON benchmark: citm_catalog.json
// Data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT
//
// Parses a CITM catalog with events, performances, and seat categories.

use { println, Stdout } from "core:cli";
use { Serialize, Deserialize } from "core:serde";
use { from_string } from "core:json";
use { TreeMap } from "core:collections";
use { MonotonicClock } from "wasi:clocks";

struct Event {
    #[serde(default)]
    description: Option<String>,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    name: String,
    #[serde(rename = "subTopicIds")]
    sub_topic_ids: Array<i64>,
    #[serde(default, rename = "subjectCode")]
    subject_code: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(rename = "topicIds")]
    topic_ids: Array<i64>,
}
impl Deserialize for Event;

struct Price {
    amount: i64,
    #[serde(rename = "audienceSubCategoryId")]
    audience_sub_category_id: i64,
    #[serde(rename = "seatCategoryId")]
    seat_category_id: i64,
}
impl Deserialize for Price;

struct Area {
    #[serde(rename = "areaId")]
    area_id: i64,
    #[serde(rename = "blockIds")]
    block_ids: Array<i64>,
}
impl Deserialize for Area;

struct SeatCategory {
    areas: Array<Area>,
    #[serde(rename = "seatCategoryId")]
    seat_category_id: i64,
}
impl Deserialize for SeatCategory;

struct Performance {
    #[serde(rename = "eventId")]
    event_id: i64,
    id: i64,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    prices: Array<Price>,
    #[serde(rename = "seatCategories")]
    seat_categories: Array<SeatCategory>,
    #[serde(default, rename = "seatMapImage")]
    seat_map_image: Option<String>,
    start: i64,
    #[serde(rename = "venueCode")]
    venue_code: String,
}
impl Deserialize for Performance;

struct CitmCatalog {
    #[serde(rename = "areaNames")]
    area_names: TreeMap<String, String>,
    #[serde(rename = "audienceSubCategoryNames")]
    audience_sub_category_names: TreeMap<String, String>,
    #[serde(rename = "blockNames")]
    block_names: TreeMap<String, String>,
    events: TreeMap<String, Event>,
    performances: Array<Performance>,
    #[serde(rename = "seatCategoryNames")]
    seat_category_names: TreeMap<String, String>,
    #[serde(rename = "subTopicNames")]
    sub_topic_names: TreeMap<String, String>,
    #[serde(rename = "subjectNames")]
    subject_names: TreeMap<String, String>,
    #[serde(rename = "topicNames")]
    topic_names: TreeMap<String, String>,
    #[serde(rename = "topicSubTopics")]
    topic_sub_topics: TreeMap<String, Array<i64>>,
    #[serde(rename = "venueNames")]
    venue_names: TreeMap<String, String>,
}
impl Deserialize for CitmCatalog;
'''

CATALOG_BENCH = r'''
export fn run() with Stdout, MonotonicClock {
    let iterations = 10;
    println(`json-catalog: {JSON_DATA.len()} bytes, {iterations} iterations`);

    let start = MonotonicClock::now();
    let mut total_events = 0;
    let mut total_performances = 0;
    for let mut i = 0; i < iterations; i += 1 {
        let result = from_string::<CitmCatalog>(JSON_DATA);
        if let Ok(catalog) = result {
            total_events = total_events + catalog.events.len();
            total_performances = total_performances + catalog.performances.len();
        } else {
            if let Err(e) = result {
                println(`Parse error: {e.message}`);
            }
            break;
        }
    }
    let end = MonotonicClock::now();
    let elapsed_ns = (end - start) as u64;
    let elapsed_ms = elapsed_ns / 1000000;
    let elapsed_us = elapsed_ns / 1000;

    assert total_events == 184 * iterations;
    assert total_performances == 243 * iterations;
    println(`Parsed {total_events} events, {total_performances} performances`);
    println(`Elapsed: {elapsed_ms}.{elapsed_us % 1000} ms`);
}
'''


BENCHMARKS = [
    ("json_twitter", "twitter.json", TWITTER_STRUCTS, TWITTER_BENCH),
    ("json_canada", "canada.json", CANADA_STRUCTS, CANADA_BENCH),
    ("json_catalog", "citm_catalog.json", CATALOG_STRUCTS, CATALOG_BENCH),
]


def generate():
    for dir_name, json_file, structs, bench in BENCHMARKS:
        json_path = os.path.join(SCRIPT_DIR, dir_name, json_file)
        out_path = os.path.join(SCRIPT_DIR, dir_name, f"{dir_name}.wado")

        with open(json_path) as f:
            raw_json = f.read()

        escaped = escape_wado_string(raw_json)

        with open(out_path, "w") as f:
            f.write(structs.lstrip("\n"))
            f.write(f'\nglobal JSON_DATA: String = "{escaped}";\n')
            f.write(bench)

        print(f"Generated {out_path} ({len(raw_json)} bytes JSON)")


if __name__ == "__main__":
    generate()
