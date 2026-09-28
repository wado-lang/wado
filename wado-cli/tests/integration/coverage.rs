//! `wado test --coverage`: the flags, the report files, and plans merged across
//! the test files that reach one module. What a region is lives in the e2e
//! fixtures (`coverage_*.wado`).

use std::fs;
use std::path::Path;

use predicates::prelude::*;

use crate::common::wado_in;

/// A package whose two test files each run one of `lib.wado`'s functions.
fn write_package(root: &Path) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"cov\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("lib.wado"),
        "pub fn one() -> i32 {\n    return 1;\n}\n\npub fn two(x: i32) -> i32 {\n    if x > 0 {\n        return 2;\n    }\n    return 0;\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("one_test.wado"),
        "use { one } from \"./lib.wado\";\n\ntest \"one\" {\n    assert one() == 1;\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("two_test.wado"),
        "use { two } from \"./lib.wado\";\n\ntest \"two\" {\n    assert two(1) == 2;\n}\n",
    )
    .unwrap();
}

#[test]
fn plans_merge_across_test_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    wado_in(tmp.path())
        .args(["test", "--coverage=lcov,json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "coverage: lines 3/4 (75.0%), branches 1/2 (50.0%), functions 2/2 (100.0%)",
        ))
        .stdout(predicate::str::contains("  lib.wado "));

    let lcov = fs::read_to_string(tmp.path().join("build/coverage/lcov.info")).unwrap();
    assert!(lcov.contains("SF:lib.wado\n"), "{lcov}");
    assert!(lcov.contains("FNDA:1,one\n"), "{lcov}");
    assert!(lcov.contains("FNDA:1,two\n"), "{lcov}");
    assert!(lcov.contains("BRDA:6,0,1,0\n"), "{lcov}");
    assert!(lcov.contains("DA:9,0\n"), "{lcov}");

    let json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tmp.path().join("build/coverage/coverage.json")).unwrap(),
    )
    .unwrap();
    let lib = json["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "lib.wado")
        .unwrap();
    assert_eq!(lib["uncovered_lines"], serde_json::json!([9]));
    let body_of_one = &lib["regions"][0];
    assert_eq!(body_of_one["function"], "one");
    assert_eq!(
        body_of_one["tests"],
        serde_json::json!(["one_test.wado::one"])
    );
}

#[test]
fn tap_carries_the_summary_as_comments() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    let out = wado_in(tmp.path())
        .args(["test", "--coverage", "--format", "tap"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("] coverage: lines 3/4"), "{out}");
    let not_tap: Vec<&str> = out
        .lines()
        .filter(|line| {
            !["TAP version", "1..", "ok ", "not ok ", "#", "    "]
                .iter()
                .any(|start| line.starts_with(start))
        })
        .collect();
    assert!(not_tap.is_empty(), "{not_tap:?}");
}

#[test]
fn a_baseline_holds_the_regions_left_unrun_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    wado_in(tmp.path())
        .args(["test", "--coverage=baseline"])
        .assert()
        .success();
    let written = fs::read_to_string(tmp.path().join("build/coverage/baseline.json")).unwrap();
    let baseline: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        baseline,
        serde_json::json!({ "lib.wado": { "two": ["else 2", "rest 3"] } })
    );

    let path = tmp.path().join("baseline.json");
    fs::write(&path, &written).unwrap();
    wado_in(tmp.path())
        .args(["test", "--coverage", "--coverage-baseline", "baseline.json"])
        .assert()
        .success();

    fs::write(
        &path,
        r#"{ "lib.wado": { "one": ["fn 0"], "two": ["else 2"] } }"#,
    )
    .unwrap();
    wado_in(tmp.path())
        .args(["test", "--coverage", "--coverage-baseline", "baseline.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "not run, not in the baseline: lib.wado: two: rest 3",
        ))
        .stderr(predicate::str::contains(
            "run now, remove from the baseline: lib.wado: one: fn 0",
        ));
}

#[test]
fn coverage_flags_need_coverage() {
    let tmp = tempfile::tempdir().unwrap();
    write_package(tmp.path());

    wado_in(tmp.path())
        .args(["test", "--coverage-include", "deps"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--coverage-include needs --coverage",
        ));
    wado_in(tmp.path())
        .args(["test", "--coverage-baseline", "b.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--coverage-baseline needs --coverage",
        ));
    wado_in(tmp.path())
        .args(["test", "--coverage=html"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--coverage takes lcov, json and baseline, got 'html'",
        ));
    wado_in(tmp.path())
        .args(["test", "--coverage", "--coverage-include", "all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--coverage-include takes deps and stdlib, got 'all'",
        ));
    wado_in(tmp.path())
        .args(["test", "--coverage", "--no-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--coverage has nothing to measure under --no-run",
        ));
}
