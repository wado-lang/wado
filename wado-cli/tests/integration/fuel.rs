//! `wado test --report-fuel` with `core:fuel`: the regions a test measures,
//! on its result line and in `build/fuel.json`.

use std::fs;
use std::path::Path;

use predicates::prelude::*;

use crate::common::wado_in;

/// A package whose one test measures a short loop and then a longer one.
fn write_package(root: &Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"fuel\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("loops_test.wado"),
        r#"use { measure } from "core:fuel";

fn count(n: i32) -> i32 {
    let mut s = 0;
    for let i of 0..<builtin::black_box(n) {
        s += i;
    }
    return s;
}

test "loops" {
    assert measure("short", || count(10)) == 45;
    assert measure("long", || count(1000)) == 499_500;
}
"#,
    )
    .unwrap();
}

fn report(root: &Path) -> String {
    wado_in(root)
        .args(["test", "--format", "verbose", "--report-fuel"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"ok   loops \(\S+, fuel \d+; short \d+; long \d+\)").unwrap());
    fs::read_to_string(root.join("build/fuel.json")).unwrap()
}

#[test]
fn regions_are_reported_in_order_and_reproduce() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    let first = report(tmp.path());
    let json: serde_json::Value = serde_json::from_str(&first).unwrap();
    let [entry] = json.as_array().unwrap().as_slice() else {
        panic!("one metered test: {first}");
    };
    assert_eq!(entry["file"], "loops_test.wado");
    assert_eq!(entry["test"], "loops");
    let regions = entry["regions"].as_array().unwrap();
    let labels: Vec<_> = regions.iter().map(|r| r["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["short", "long"]);
    let fuel = |i: usize| regions[i]["fuel"].as_u64().unwrap();
    assert!(fuel(0) < fuel(1), "{first}");
    assert!(fuel(0) + fuel(1) < entry["fuel"].as_u64().unwrap(), "{first}");

    assert_eq!(report(tmp.path()), first);
}

#[test]
fn an_unmetered_run_just_runs_the_closure() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    wado_in(tmp.path())
        .args(["test", "--format", "verbose"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok   loops"));
    assert!(!tmp.path().join("build/fuel.json").exists());
}
