//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them with wasmtime,
//! verifying the output matches expected values defined in each file's __DATA__ section.
//!
//! Test fixtures in fixtures/*.wado must have a __DATA__ section with JSON specifying
//! expected results. Helper modules that are imported by tests go in subdirectories
//! (e.g., fixtures/sub/) and are not run as tests themselves.

mod common;

use serde::Deserialize;
use std::path::Path;
use wado_compiler::OptLevel;

// ============================================================================
// Test Spec
// ============================================================================

/// Expected test results from __DATA__ section (JSON format)
#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    /// Expected stdout (exact match)
    #[serde(default)]
    stdout: Option<String>,

    /// Expected stderr (exact match)
    #[serde(default)]
    stderr: Option<String>,

    /// Strings that must be contained in stdout
    #[serde(default)]
    stdout_contains: Vec<String>,

    /// Strings that must be contained in stderr
    #[serde(default)]
    stderr_contains: Vec<String>,

    /// Whether the program is expected to trap
    #[serde(default)]
    trapped: bool,

    /// Expected compile error message (substring match).
    /// If set, the test expects compilation to fail with this message.
    #[serde(default)]
    compile_error: Option<String>,

    /// Whether this is a TODO test (not yet implemented feature).
    /// TODO tests MUST fail (compile error, runtime error, or wrong output).
    /// If a TODO test passes, the test will fail to remind you to remove the TODO flag.
    #[serde(default)]
    #[serde(rename = "TODO")]
    todo: bool,

    /// Skip this test in O0 mode (no optimization).
    /// Use for tests that require O2+ features like DCE-based function discovery
    /// (e.g., tests using wasi:sockets which requires DCE to find function references).
    #[serde(default)]
    skip_o0: bool,
}

// ============================================================================
// Test Verification
// ============================================================================

/// Verify the actual result matches the expected spec
fn verify_result(result: &common::WasmRunResult, spec: &TestSpec, fixture_name: &str) {
    // Check trapped status
    assert_eq!(
        result.trapped, spec.trapped,
        "[{fixture_name}] trapped mismatch: expected {}, got {}",
        spec.trapped, result.trapped
    );

    // Check stdout exact match if specified
    if let Some(expected_stdout) = &spec.stdout {
        assert_eq!(
            &result.stdout, expected_stdout,
            "[{fixture_name}] stdout mismatch"
        );
    }

    // Check stderr exact match if specified
    if let Some(expected_stderr) = &spec.stderr {
        assert_eq!(
            &result.stderr, expected_stderr,
            "[{fixture_name}] stderr mismatch"
        );
    }

    // Check stdout contains
    for expected in &spec.stdout_contains {
        assert!(
            result.stdout.contains(expected),
            "[{fixture_name}] stdout should contain '{expected}', but got:\n{}",
            result.stdout
        );
    }

    // Check stderr contains
    for expected in &spec.stderr_contains {
        assert!(
            result.stderr.contains(expected),
            "[{fixture_name}] stderr should contain '{expected}', but got:\n{}",
            result.stderr
        );
    }
}

// ============================================================================
// Test Runner
// ============================================================================

/// Run a single fixture test at a specific optimization level
fn run_fixture_test_with_opt(fixture_path: &Path, opt_level: OptLevel) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let opt_name = common::opt_level_name(opt_level);
    let test_id = format!("{fixture_name} ({opt_name})");

    // Read the source file to extract __DATA__ section before compilation
    let source = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!("[{test_id}] failed to read file: {e}");
    });

    // Get the __DATA__ section - required for all fixtures
    let data_section = common::extract_data_section(&source).unwrap_or_else(|| {
        panic!("[{test_id}] missing __DATA__ section - all fixtures must have test expectations");
    });

    // Parse the test spec from JSON
    let spec: TestSpec = common::parse_data_section(data_section, &test_id);

    // Skip O0 tests if skip_o0 is set (for tests requiring DCE-based features)
    if spec.skip_o0 && opt_level == OptLevel::O0 {
        eprintln!("[{test_id}] skipped (requires O2+ optimization)");
        return;
    }

    // Handle TODO tests - they must fail
    if spec.todo {
        eprintln!("[{test_id}] TODO test - expecting failure");

        // Use catch_unwind to recover from panics
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_normal_test(fixture_path, opt_level, &spec, &test_id)
        }));

        match test_result {
            Ok(()) => {
                // Test passed, but it's a TODO test, so it should have failed!
                panic!(
                    "[{test_id}] TODO test PASSED! This means the feature is now implemented.\n\
                     Please remove 'TODO: true' from the __DATA__ section."
                );
            }
            Err(err) => {
                // Test failed as expected for a TODO test
                let msg = err
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| err.downcast_ref::<&str>().copied())
                    .unwrap_or("(unknown panic)");

                eprintln!("[{test_id}] TODO test failed as expected (feature not yet implemented)");
                eprintln!("[{test_id}] Error: {msg}");
                return;
            }
        }
    }

    // Normal test - run without panic recovery
    run_normal_test(fixture_path, opt_level, &spec, &test_id);
}

/// Run a normal (non-TODO) test
fn run_normal_test(fixture_path: &Path, opt_level: OptLevel, spec: &TestSpec, test_id: &str) {
    // Try to compile the fixture
    let compile_result = common::compile_file_with_opts(fixture_path, opt_level);

    // Handle expected compile errors
    if let Some(expected_error) = &spec.compile_error {
        match compile_result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains(expected_error),
                    "[{test_id}] compile error mismatch:\n  expected to contain: {expected_error}\n  actual error: {error_msg}"
                );
                return; // Test passed - expected compile error occurred
            }
            Ok(_) => {
                panic!(
                    "[{test_id}] expected compile error containing '{expected_error}', but compilation succeeded"
                );
            }
        }
    }

    // No compile error expected - compilation must succeed
    let compile_result = compile_result.unwrap_or_else(|e| {
        panic!("[{test_id}] compilation failed: {e}");
    });

    // Run and capture output
    let result = common::run_wasm(compile_result.wasm).unwrap_or_else(|e| {
        panic!("[{test_id}] runtime error: {e}");
    });

    // Verify the result matches expectations
    verify_result(&result, spec, test_id);
}

// ============================================================================
// Test Entry Points
// ============================================================================

/// Test function for O0 (no optimization)
fn fixture_test_o0(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::O0);
    Ok(())
}

/// Test function for O2 (full optimization)
fn fixture_test_o2(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::O2);
    Ok(())
}

/// Test function for O3 (aggressive optimization)
fn fixture_test_o3(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::O3);
    Ok(())
}

/// Test function for Os (size optimization)
fn fixture_test_os(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::Os);
    Ok(())
}

datatest_mini::harness! {
    // Pattern matches .wado files directly in fixtures/ but not in subdirectories
    // (subdirectories contain helper modules that are imported, not run as tests)
    { test = fixture_test_o0, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o2, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o3, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_os, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
}
