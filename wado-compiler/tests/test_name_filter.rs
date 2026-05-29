//! `--test-name` compile-time filtering and the `wado:test-names` section.
//!
//! `wado test --test-name <pattern>` is implemented by passing the substring
//! filters into `CompilerOptions::test_name_filters`. Only `test "name"` blocks
//! whose name contains a filter are exported from the test-world component; the
//! rest lose their `is_cm_export` adapter and are removed by early DCE, so they
//! are never present in the output. The compiler also writes a
//! `wado:test-names` custom section mapping each surviving export to its
//! original (lossless) name for the runner to display.
//!
//! These tests inspect the compiled component bytes to hold both contracts
//! honest: the surviving test exports and the names recorded in the section.

mod common;

use std::path::Path;

const SOURCE: &str = r#"
test "alpha addition" {
    assert 1 + 1 == 2;
}

test "beta subtraction" {
    assert 3 - 1 == 2;
}

test "alpha multiplication" {
    assert 2 * 3 == 6;
}

test "日本語 ok" {
    assert true;
}
"#;

/// Compile `SOURCE` to the test world with the given `--test-name` filters and
/// return the `(export_name, original_name)` pairs recorded in the
/// `wado:test-names` custom section. The section enumerates exactly the test
/// exports that survived DCE, so it doubles as the surviving-export set.
fn compiled_test_names(filters: &[&str]) -> Vec<(String, Option<String>)> {
    let options = wado_compiler::CompilerOptions {
        target_world: Some("test".to_string()),
        test_name_filters: filters.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    };
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/__test_name_filter__.wado");
    let result = common::compile_source_with_compiler_options(&path, SOURCE, options)
        .expect("compile should succeed");
    // The section is present whenever at least one test survives; a fully
    // filtered-out build exports no tests and carries no section, which reads
    // back as an empty set.
    let mut names =
        wado_compiler::test_names::read_from_component(&result.wasm).unwrap_or_default();
    names.sort();
    names
}

/// Just the original (source) names recorded for the surviving exports.
fn original_names(filters: &[&str]) -> Vec<String> {
    compiled_test_names(filters)
        .into_iter()
        .filter_map(|(_, name)| name)
        .collect()
}

#[test]
fn no_filter_keeps_every_test() {
    let mut names = original_names(&[]);
    names.sort();
    assert_eq!(
        names,
        vec![
            "alpha addition".to_string(),
            "alpha multiplication".to_string(),
            "beta subtraction".to_string(),
            "日本語 ok".to_string(),
        ]
    );
}

#[test]
fn substring_filter_keeps_only_matching_tests() {
    let mut names = original_names(&["alpha"]);
    names.sort();
    assert_eq!(
        names,
        vec![
            "alpha addition".to_string(),
            "alpha multiplication".to_string(),
        ],
        "only the two `alpha` tests should survive; the rest are DCE'd"
    );
}

#[test]
fn multiple_filters_combine_with_or() {
    let mut names = original_names(&["beta", "multiplication"]);
    names.sort();
    assert_eq!(
        names,
        vec![
            "alpha multiplication".to_string(),
            "beta subtraction".to_string(),
        ]
    );
}

#[test]
fn multibyte_filter_matches_original_name() {
    // The kebab export folds non-ASCII away (`test-3` here), so matching has to
    // run against the lossless original name carried in the section.
    assert_eq!(original_names(&["日本語"]), vec!["日本語 ok".to_string()]);
}

#[test]
fn non_matching_filter_keeps_nothing() {
    assert!(
        compiled_test_names(&["no-such-test"]).is_empty(),
        "a filter that matches no test should export no tests"
    );
}

#[test]
fn section_records_lossless_names_for_kebab_folded_exports() {
    // `日本語 ok` folds to the bare `test-3` export name; the section is the
    // only place the original survives.
    let names = compiled_test_names(&["日本語"]);
    assert_eq!(names.len(), 1);
    let (export_name, original) = &names[0];
    assert!(
        export_name.is_ascii(),
        "export name must be ASCII kebab: {export_name:?}"
    );
    assert_eq!(original.as_deref(), Some("日本語 ok"));
}
