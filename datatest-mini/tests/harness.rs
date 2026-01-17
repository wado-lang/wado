//! Integration test for datatest_mini::harness! macro
//!
//! This test verifies that the harness! macro correctly:
//! - Generates test functions for matching files
//! - Respects the regex pattern (only .txt files starting with "test_")
//! - Excludes files in subdirectories (due to `^...$` anchoring)
//! - Excludes files that don't match the pattern (skip_me.json)

use std::path::Path;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn run_fixture(path: &Path) -> TestResult {
    // Verify the path exists
    assert!(path.exists(), "path should exist: {}", path.display());

    // Verify it's a .txt file
    assert!(
        path.extension().is_some_and(|ext| ext == "txt"),
        "should only match .txt files, got: {}",
        path.display()
    );

    // Verify it's not in a subdirectory (pattern should exclude those)
    assert!(
        !path.to_str().unwrap().contains("subdir"),
        "should not match files in subdirectories: {}",
        path.display()
    );

    // Verify filename starts with "test_"
    let file_name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        file_name.starts_with("test_"),
        "filename should start with test_: {}",
        file_name
    );

    // Read and verify content
    let content = std::fs::read_to_string(path)?;
    assert!(!content.is_empty(), "file should have content");

    Ok(())
}

// This generates test functions for each .txt file at root that starts with "test_"
// Expected: fixtures::test_a_txt, fixtures::test_b_txt (2 tests)
// Excluded: skip_me.json (wrong extension), subdir/nested.txt (in subdirectory)
datatest_mini::harness! {
    { test = run_fixture, root = "tests/fixtures", pattern = r"^test_[^/]+\.txt$" },
}
