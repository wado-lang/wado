//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them with wasmtime,
//! verifying the output matches expected values defined in each file's __DATA__ section.
//!
//! Test fixtures in fixtures/*.wado must have a __DATA__ section with JSON specifying
//! expected results. Library files that are imported by tests go in fixtures/lib/.

use serde::Deserialize;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use std::path::Path;
use wado_compiler::compile_file;

struct TestWasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for TestWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Result of running a Wasm component with captured output
#[derive(Debug)]
struct WasmRunResult {
    /// Captured stdout
    stdout: String,
    /// Captured stderr
    stderr: String,
    /// Whether the component trapped (e.g., from unreachable)
    trapped: bool,
}

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
}

fn run_wasm(wasm: Vec<u8>) -> anyhow::Result<WasmRunResult> {
    // Create a new tokio runtime for this test
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let mut config = Config::new();
        config.async_support(true);
        config.wasm_component_model(true);
        config.wasm_component_model_gc(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_async_builtins(true);
        config.wasm_component_model_async_stackful(true);
        config.wasm_simd(true);
        config.wasm_wide_arithmetic(true);
        config.wasm_threads(true);
        config.wasm_gc(true);
        config.wasm_function_references(true);

        let engine = Engine::new(&config)?;

        // Create component from wasm bytes
        let component = Component::new(&engine, &wasm)?;

        // Set up linker with WASI P3
        let mut linker: Linker<TestWasiState> = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;

        // Create stdout and stderr capture pipes
        let stdout_pipe = MemoryOutputPipe::new(4096);
        let stdout_clone = stdout_pipe.clone();
        let stderr_pipe = MemoryOutputPipe::new(4096);
        let stderr_clone = stderr_pipe.clone();

        // Create WASI state with captured stdout and stderr
        let ctx = WasiCtxBuilder::new()
            .stdout(stdout_pipe)
            .stderr(stderr_pipe)
            .build();
        let table = ResourceTable::new();

        let state = TestWasiState { ctx, table };
        let mut store = Store::new(&engine, state);

        // Instantiate the component
        let instance = linker.instantiate_async(&mut store, &component).await?;

        // Get and call the "run" function
        let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

        let trapped = match run_func.call_async(&mut store, ()).await {
            Ok((result,)) => result.is_err(),
            Err(_) => true, // Runtime error (trap)
        };

        // Get captured output
        let stdout_bytes = stdout_clone.contents();
        let stdout = String::from_utf8(stdout_bytes.to_vec())?;
        let stderr_bytes = stderr_clone.contents();
        let stderr = String::from_utf8(stderr_bytes.to_vec())?;

        Ok(WasmRunResult {
            stdout,
            stderr,
            trapped,
        })
    })
}

/// Extract __DATA__ section from source file content
fn extract_data_section(source: &str) -> Option<&str> {
    let marker = "\n__DATA__\n";
    if let Some(pos) = source.find(marker) {
        Some(&source[pos + marker.len()..])
    } else if source.starts_with("__DATA__\n") {
        Some(&source["__DATA__\n".len()..])
    } else {
        None
    }
}

/// Parse test spec from __DATA__ section JSON
fn parse_test_spec(data_section: &str, fixture_name: &str) -> TestSpec {
    serde_json::from_str(data_section).unwrap_or_else(|e| {
        panic!("[{fixture_name}] Failed to parse __DATA__ section as JSON: {e}\nContent:\n{data_section}");
    })
}

/// Verify the actual result matches the expected spec
fn verify_result(result: &WasmRunResult, spec: &TestSpec, fixture_name: &str) {
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

/// Run a single fixture test by path
fn run_fixture_test(fixture_path: &Path) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    // Read the source file to extract __DATA__ section before compilation
    let source = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!("[{fixture_name}] failed to read file: {e}");
    });

    // Get the __DATA__ section - required for all fixtures
    let data_section = extract_data_section(&source).unwrap_or_else(|| {
        panic!(
            "[{fixture_name}] missing __DATA__ section - all fixtures must have test expectations"
        );
    });

    // Parse the test spec from JSON
    let spec = parse_test_spec(data_section, &fixture_name);

    // Try to compile the fixture
    let compile_result = compile_file(fixture_path);

    // Handle expected compile errors
    if let Some(expected_error) = &spec.compile_error {
        match compile_result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains(expected_error),
                    "[{fixture_name}] compile error mismatch:\n  expected to contain: {expected_error}\n  actual error: {error_msg}"
                );
                return; // Test passed - expected compile error occurred
            }
            Ok(_) => {
                panic!(
                    "[{fixture_name}] expected compile error containing '{expected_error}', but compilation succeeded"
                );
            }
        }
    }

    // No compile error expected - compilation must succeed
    let compile_result = compile_result.unwrap_or_else(|e| {
        panic!("[{fixture_name}] compilation failed: {e}");
    });

    // Run and capture output
    let result = run_wasm(compile_result.wasm).unwrap_or_else(|e| {
        panic!("[{fixture_name}] runtime error: {e}");
    });

    // Verify the result matches expectations
    verify_result(&result, &spec, &fixture_name);
}

/// Test function for datatest-stable - runs each .wado fixture file
fn fixture_test(path: &Path) -> datatest_stable::Result<()> {
    run_fixture_test(path);
    Ok(())
}

datatest_stable::harness! {
    { test = fixture_test, root = "tests/fixtures", pattern = r"\.wado$" },
}
