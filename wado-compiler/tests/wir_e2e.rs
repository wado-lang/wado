//! WIR pipeline E2E tests — parallel test harness for the WIR backend.
//!
//! These tests run the same fixtures as `e2e.rs` but use the WIR pipeline
//! (`tir_to_wir` → `wir_emit`) instead of `Codegen::generate_wasm`.
//!
//! Gated by `WADO_WIR_TEST=1` — not included in normal `make test`.
//!
//! Usage:
//!   WADO_WIR_TEST=1 cargo test -p wado-compiler --test wir_e2e

mod common;

use serde::Deserialize;
use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

// ============================================================================
// Gate: skip all tests unless WADO_WIR_TEST=1
// ============================================================================

fn wir_tests_enabled() -> bool {
    std::env::var("WADO_WIR_TEST").is_ok()
}

// ============================================================================
// Test Spec (same as e2e.rs)
// ============================================================================

#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stderr_contains: Vec<String>,
    #[serde(default)]
    trapped: bool,
    #[serde(default)]
    compile_error: Option<String>,
    #[serde(default)]
    #[serde(rename = "TODO")]
    todo: bool,
}

// ============================================================================
// Test Verification (same as e2e.rs)
// ============================================================================

fn verify_result(result: &common::WasmRunResult, spec: &TestSpec, fixture_name: &str) {
    assert_eq!(
        result.trapped, spec.trapped,
        "[{fixture_name}] trapped mismatch: expected {}, got {}",
        spec.trapped, result.trapped
    );

    if let Some(expected_stdout) = &spec.stdout {
        assert_eq!(
            &result.stdout, expected_stdout,
            "[{fixture_name}] stdout mismatch"
        );
    }

    if let Some(expected_stderr) = &spec.stderr {
        assert_eq!(
            &result.stderr, expected_stderr,
            "[{fixture_name}] stderr mismatch"
        );
    }

    for expected in &spec.stdout_contains {
        assert!(
            result.stdout.contains(expected),
            "[{fixture_name}] stdout should contain '{expected}', but got:\n{}",
            result.stdout
        );
    }

    for expected in &spec.stderr_contains {
        assert!(
            result.stderr.contains(expected),
            "[{fixture_name}] stderr should contain '{expected}', but got:\n{}",
            result.stderr
        );
    }
}

// ============================================================================
// WIR Compilation Helper
// ============================================================================

fn compile_with_wir_backend(
    path: &Path,
    source: &str,
    opt_level: OptLevel,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileError> {
    let options = CompilerOptions {
        opt_level,
        use_wir_backend: true,
        ..CompilerOptions::default()
    };
    common::compile_source_with_compiler_options(path, source, options)
}

// ============================================================================
// Test Runner
// ============================================================================

fn run_fixture_test_with_opt(fixture_path: &Path, source: &str, opt_level: OptLevel) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let opt_name = common::opt_level_name(opt_level);
    let test_id = format!("{fixture_name} [WIR] ({opt_name})");

    let data_section = common::extract_data_section(source).unwrap_or_else(|| {
        panic!("[{test_id}] missing __DATA__ section");
    });

    let spec: TestSpec = common::parse_data_section(data_section, &test_id);

    // Handle TODO tests
    if spec.todo {
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
        }));

        match test_result {
            Ok(()) => {
                panic!(
                    "[{test_id}] TODO test PASSED! This means the feature is now implemented.\n\
                     Please remove 'TODO: true' from the __DATA__ section."
                );
            }
            Err(_) => {
                return;
            }
        }
    }

    run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
}

fn run_normal_test(
    fixture_path: &Path,
    source: &str,
    opt_level: OptLevel,
    spec: &TestSpec,
    test_id: &str,
) {
    let compile_result = compile_with_wir_backend(fixture_path, source, opt_level);

    // Handle expected compile errors
    if let Some(expected_error) = &spec.compile_error {
        match compile_result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains(expected_error),
                    "[{test_id}] compile error mismatch:\n  expected to contain: {expected_error}\n  actual error: {error_msg}"
                );
                return;
            }
            Ok(_) => {
                panic!(
                    "[{test_id}] expected compile error containing '{expected_error}', but compilation succeeded"
                );
            }
        }
    }

    let compile_result = compile_result.unwrap_or_else(|e| {
        panic!("[{test_id}] compilation failed: {e}");
    });

    let result = common::run_wasm(compile_result.wasm).unwrap_or_else(|e| {
        panic!("[{test_id}] runtime error: {e}");
    });

    verify_result(&result, spec, test_id);
}

// ============================================================================
// Test Entry Points (O0 and O2 only for WIR — keep iteration fast)
// ============================================================================

fn fixture_test_wir_o0(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !wir_tests_enabled() {
        return Ok(());
    }
    run_fixture_test_with_opt(path, content, OptLevel::O0);
    Ok(())
}

fn fixture_test_wir_o2(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !wir_tests_enabled() {
        return Ok(());
    }
    run_fixture_test_with_opt(path, content, OptLevel::O2);
    Ok(())
}

datatest_mini::harness! {
    { test = fixture_test_wir_o0, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_wir_o2, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
}
