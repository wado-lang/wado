//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them with wasmtime,
//! verifying the output matches expected values defined in each file's __DATA__ section.
//!
//! Test fixtures in fixtures/*.wado must have a __DATA__ section with JSON specifying
//! expected results. Helper modules that are imported by tests go in subdirectories
//! (e.g., fixtures/sub/) and are not run as tests themselves.

use serde::Deserialize;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use wado_compiler::CompilerHost;
use wado_compiler::{CompileError, OptLevel};

// ============================================================================
// Test Compiler Host (Filesystem-based)
// ============================================================================

/// A simple filesystem-based CompilerHost for tests
struct TestCompilerHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<wado_compiler::Diagnostic>>,
}

impl TestCompilerHost {
    fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
        }
    }
}

impl CompilerHost for TestCompilerHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<String, wado_compiler::SourceError>> + Send {
        let full_path = self.base_path.join(path);
        async move {
            std::fs::read_to_string(&full_path).map_err(|e| wado_compiler::SourceError::IoError {
                path: full_path.to_string_lossy().to_string(),
                message: e.to_string(),
            })
        }
    }

    fn emit_diagnostic(
        &self,
        diagnostic: wado_compiler::Diagnostic,
    ) -> impl std::future::Future<Output = ()> + Send {
        let diagnostics = &self.diagnostics;
        async move {
            diagnostics.lock().unwrap().push(diagnostic);
        }
    }
}

/// Compile a file using the test host
fn compile_file_with_opts(
    path: &Path,
    opt_level: OptLevel,
) -> Result<wado_compiler::CompileResult, CompileError> {
    // Read source file
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.to_string_lossy().to_string(),
        message: e.to_string(),
    })?;

    // Get base path for relative imports
    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = TestCompilerHost::new(base_path);

    // Compile using async API (use tokio runtime)
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(wado_compiler::compile_with_host(
            &source,
            &host,
            Some(&path.to_string_lossy()),
            opt_level,
        ))
}

/// Shared wasmtime Engine for all tests (initialized once)
static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Shared tokio runtime for all tests (initialized once)
static TOKIO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Get or initialize the shared wasmtime Engine
fn get_engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
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

        // Use minimal optimization for faster compilation in tests
        // This reduces Wasm compilation time while maintaining compatibility with all features
        config.cranelift_opt_level(wasmtime::OptLevel::None);

        Engine::new(&config).expect("Failed to create wasmtime Engine")
    })
}

/// Get or initialize the shared tokio runtime
fn get_runtime() -> &'static tokio::runtime::Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime")
    })
}

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

    /// Whether this is a TODO test (not yet implemented feature).
    /// TODO tests MUST fail (compile error, runtime error, or wrong output).
    /// If a TODO test passes, the test will fail to remind you to remove the TODO flag.
    #[serde(default)]
    #[serde(rename = "TODO")]
    todo: bool,
}

fn run_wasm(wasm: Vec<u8>) -> anyhow::Result<WasmRunResult> {
    // Use shared runtime and engine
    let rt = get_runtime();
    let engine = get_engine();

    rt.block_on(async {
        // Create component from wasm bytes
        let component = Component::new(engine, &wasm)?;

        // Set up linker with WASI P3
        let mut linker: Linker<TestWasiState> = Linker::new(engine);
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
        let mut store = Store::new(engine, state);

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

/// Get human-readable name for optimization level
fn opt_level_name(opt: OptLevel) -> &'static str {
    match opt {
        OptLevel::None => "O0",
        OptLevel::Basic => "O1",
        OptLevel::Full => "O2",
        OptLevel::Size => "Os",
    }
}

/// Run a single fixture test at a specific optimization level
fn run_fixture_test_with_opt(fixture_path: &Path, opt_level: OptLevel) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let opt_name = opt_level_name(opt_level);
    let test_id = format!("{fixture_name} ({opt_name})");

    // Read the source file to extract __DATA__ section before compilation
    let source = std::fs::read_to_string(fixture_path).unwrap_or_else(|e| {
        panic!("[{test_id}] failed to read file: {e}");
    });

    // Get the __DATA__ section - required for all fixtures
    let data_section = extract_data_section(&source).unwrap_or_else(|| {
        panic!("[{test_id}] missing __DATA__ section - all fixtures must have test expectations");
    });

    // Parse the test spec from JSON
    let spec = parse_test_spec(data_section, &test_id);

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
                // Extract panic message for better diagnostics
                let panic_msg = if let Some(s) = err.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = err.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    format!("{:?}", err)
                };

                eprintln!("[{test_id}] TODO test failed as expected (feature not yet implemented)");
                eprintln!("[{test_id}] Error: {}", panic_msg);
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
    let compile_result = compile_file_with_opts(fixture_path, opt_level);

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
    let result = run_wasm(compile_result.wasm).unwrap_or_else(|e| {
        panic!("[{test_id}] runtime error: {e}");
    });

    // Verify the result matches expectations
    verify_result(&result, spec, test_id);
}

/// Test function for O0 (no optimization)
fn fixture_test_o0(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::None);
    Ok(())
}

/// Test function for O2 (full optimization)
fn fixture_test_o2(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::Full);
    Ok(())
}

/// Test function for Os (size optimization)
fn fixture_test_os(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, OptLevel::Size);
    Ok(())
}

datatest_mini::harness! {
    // Pattern matches .wado files directly in fixtures/ but not in subdirectories
    // (subdirectories contain helper modules that are imported, not run as tests)
    { test = fixture_test_o0, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o2, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_os, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
}
