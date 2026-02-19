//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them,
//! verifying the output matches expected values defined in each file's __DATA__ section.
//!
//! Test fixtures support different target worlds via the `"world"` field:
//! - (default) `wasi:cli/command` — runs as CLI program, checks stdout/stderr
//! - `"wasi:http/service"` — runs as HTTP service, checks `http_status`/`body`
//! - `"test"` — runs test block exports, checks all tests pass
//!
//! Helper modules that are imported by tests go in subdirectories
//! (e.g., fixtures/sub/) and are not run as tests themselves.

mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::Store;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::p3::{Request, WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use wado_compiler::{CompilerOptions, OptLevel};

// ---------------------------------------------------------------------------
// __DATA__ spec
// ---------------------------------------------------------------------------

/// Expected test results from __DATA__ section (JSON format)
#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    /// Target world. Omit or `null` for `wasi:cli/command` (default).
    /// Use `"wasi:http/service"` for HTTP tests, `"test"` for test-block tests.
    #[serde(default)]
    world: Option<String>,

    /// Expected stdout (exact match) — CLI / test worlds
    #[serde(default)]
    stdout: Option<String>,

    /// Expected stderr (exact match) — CLI world
    #[serde(default)]
    stderr: Option<String>,

    /// Strings that must be contained in stdout
    #[serde(default)]
    stdout_contains: Vec<String>,

    /// Strings that must be contained in stderr
    #[serde(default)]
    stderr_contains: Vec<String>,

    /// Whether the program is expected to trap — CLI world
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

    /// Expected HTTP status code — HTTP world
    #[serde(default)]
    http_status: Option<u16>,

    /// Expected HTTP response body (UTF-8) — HTTP world
    #[serde(default)]
    body: Option<String>,
}

// ---------------------------------------------------------------------------
// CLI world verification (shared with test world)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// HTTP world infrastructure
// ---------------------------------------------------------------------------

struct TestHttpCtx;

impl WasiHttpCtx for TestHttpCtx {}

struct HttpTestState {
    table: ResourceTable,
    wasi: wasmtime_wasi::WasiCtx,
    http: TestHttpCtx,
}

impl WasiView for HttpTestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HttpTestState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
        }
    }
}

/// Multi-threaded tokio runtime for HTTP tests (needs `run_concurrent` / `tokio::spawn`)
static HTTP_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn http_runtime() -> &'static tokio::runtime::Runtime {
    HTTP_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create HTTP tokio runtime")
    })
}

/// Result of an HTTP request to the Wasm component
struct HttpTestResult {
    status: u16,
    body: Vec<u8>,
}

/// Run an HTTP request against a compiled Wasm component
fn run_http_request(wasm: Vec<u8>) -> anyhow::Result<HttpTestResult> {
    http_runtime().block_on(run_http_request_async(wasm))
}

async fn run_http_request_async(wasm: Vec<u8>) -> anyhow::Result<HttpTestResult> {
    let engine = common::http_engine();
    let component = Component::new(engine, &wasm)
        .map_err(|e| anyhow::anyhow!("failed to create component: {e:?}"))?;

    let mut linker: Linker<HttpTestState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;

    let state = HttpTestState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().build(),
        http: TestHttpCtx,
    };
    let mut store = Store::new(engine, state);

    let service = Service::instantiate_async(&mut store, &component, &linker).await?;

    // Create a simple GET request
    let http_req = http::Request::builder()
        .uri("http://localhost/")
        .method(http::Method::GET)
        .body(Empty::<Bytes>::new())?;

    let (req, io) = Request::from_http(http_req);

    // Channel to receive the response
    let (tx, rx) = tokio::sync::oneshot::channel();

    let timeout_duration = Duration::from_secs(1);

    let handler_result = tokio::time::timeout(timeout_duration, async {
        store
            .run_concurrent(async |store| {
                let (res, task) = match service.handle(store, req).await? {
                    Ok(pair) => pair,
                    Err(err) => return anyhow::Ok(Err(Some(err))),
                };
                let _ = tx.send(store.with(|store| res.into_http(store, async { Ok(()) }))?);
                task.block(store).await;
                Ok(Ok(()))
            })
            .await?
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP handler timed out after {timeout_duration:?}"))??;

    drop(io);

    match handler_result {
        Ok(()) => {
            let res = rx
                .await
                .map_err(|_| anyhow::anyhow!("response channel closed unexpectedly"))?;
            let status = res.status().as_u16();
            let body = res.into_body().collect().await?.to_bytes().to_vec();
            Ok(HttpTestResult { status, body })
        }
        Err(Some(error_code)) => Ok(HttpTestResult {
            status: 500,
            body: format!("{error_code:?}").into_bytes(),
        }),
        Err(None) => Err(anyhow::anyhow!("Handler returned error without error code")),
    }
}

/// Verify an HTTP test result matches the spec
fn verify_http_result(result: &HttpTestResult, spec: &TestSpec, fixture_name: &str) {
    if let Some(expected_status) = spec.http_status {
        assert_eq!(
            result.status, expected_status,
            "[{fixture_name}] HTTP status mismatch: expected {expected_status}, got {}",
            result.status
        );
    }

    if let Some(expected_body) = &spec.body {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert_eq!(
            actual_body, expected_body.as_str(),
            "[{fixture_name}] body mismatch"
        );
    }
}

// ---------------------------------------------------------------------------
// Test world runner
// ---------------------------------------------------------------------------

/// Run all test exports from a Wasm component compiled with the `test` world.
/// Each test function is called in its own Store. All tests must pass.
fn run_test_world(wasm: &[u8], test_id: &str) -> anyhow::Result<common::WasmRunResult> {
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    let rt = common::runtime();
    let engine = common::cli_engine();

    rt.block_on(async {
        let component = Component::new(engine, wasm)?;
        let linker = common::cli_linker(engine)?;

        // Find test exports
        let component_ty = component.component_type();
        let mut test_names: Vec<String> = component_ty
            .exports(engine)
            .filter_map(|(name, _)| {
                if name.starts_with("test-") {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect();
        test_names.sort();

        anyhow::ensure!(!test_names.is_empty(), "no test exports found");

        let mut all_stdout = String::new();
        let mut all_stderr = String::new();

        for test_name in &test_names {
            let stdout_pipe = MemoryOutputPipe::new(65536);
            let stdout_clone = stdout_pipe.clone();
            let stderr_pipe = MemoryOutputPipe::new(65536);
            let stderr_clone = stderr_pipe.clone();

            let state = common::CliWasiState::new_with_pipes(stdout_pipe, stderr_pipe);
            let mut store = Store::new(engine, state);

            let instance = linker.instantiate_async(&mut store, &component).await?;
            let func =
                instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, test_name)?;

            match func.call_async(&mut store, ()).await {
                Ok((Ok(()),)) => {} // passed
                Ok((Err(()),)) => {
                    let stderr_text =
                        String::from_utf8_lossy(&stderr_clone.contents()).to_string();
                    anyhow::bail!(
                        "[{test_id}] test '{test_name}' returned error. stderr: {stderr_text}"
                    );
                }
                Err(e) => {
                    let stderr_text =
                        String::from_utf8_lossy(&stderr_clone.contents()).to_string();
                    anyhow::bail!(
                        "[{test_id}] test '{test_name}' trapped: {e}. stderr: {stderr_text}"
                    );
                }
            }

            all_stdout.push_str(&String::from_utf8_lossy(&stdout_clone.contents()));
            all_stderr.push_str(&String::from_utf8_lossy(&stderr_clone.contents()));
        }

        Ok(common::WasmRunResult {
            stdout: all_stdout,
            stderr: all_stderr,
            trapped: false,
        })
    })
}

// ---------------------------------------------------------------------------
// Main test dispatch
// ---------------------------------------------------------------------------

/// Run a single fixture test at a specific optimization level
fn run_fixture_test_with_opt(fixture_path: &Path, source: &str, opt_level: OptLevel) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let opt_name = common::opt_level_name(opt_level);
    let test_id = format!("{fixture_name} ({opt_name})");

    // Get the __DATA__ section - required for all fixtures
    let data_section = common::extract_data_section(source).unwrap_or_else(|| {
        panic!("[{test_id}] missing __DATA__ section - all fixtures must have test expectations");
    });

    // Parse the test spec from JSON
    let spec: TestSpec = common::parse_data_section(data_section, &test_id);

    // Handle TODO tests - they must fail
    if spec.todo {
        eprintln!("[{test_id}] TODO test - expecting failure");

        // Use catch_unwind to recover from panics
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
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
                    .map(String::as_str)
                    .or_else(|| err.downcast_ref::<&str>().copied())
                    .unwrap_or("(unknown panic)");

                eprintln!("[{test_id}] TODO test failed as expected (feature not yet implemented)");
                eprintln!("[{test_id}] Error: {msg}");
                return;
            }
        }
    }

    // Normal test - run without panic recovery
    run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
}

/// Run a normal (non-TODO) test, dispatching to the appropriate world runner
fn run_normal_test(
    fixture_path: &Path,
    source: &str,
    opt_level: OptLevel,
    spec: &TestSpec,
    test_id: &str,
) {
    // Use CompilerOptions to pass the target world through
    let options = CompilerOptions {
        opt_level,
        target_world: spec.world.clone(),
        skip_validation: false,
    };

    // Try to compile the fixture
    let compile_result =
        common::compile_source_with_compiler_options(fixture_path, source, options);

    // Handle expected compile errors (works for any world)
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

    // Dispatch to the appropriate runner based on world
    match spec.world.as_deref() {
        Some("wasi:http/service") => {
            let result = run_http_request(compile_result.wasm).unwrap_or_else(|e| {
                panic!("[{test_id}] HTTP error: {e:?}");
            });
            verify_http_result(&result, spec, test_id);
        }
        Some("test") => {
            let result = run_test_world(&compile_result.wasm, test_id).unwrap_or_else(|e| {
                panic!("[{test_id}] test world error: {e:?}");
            });
            verify_result(&result, spec, test_id);
        }
        _ => {
            // Default: wasi:cli/command
            let result = common::run_wasm(compile_result.wasm).unwrap_or_else(|e| {
                panic!("[{test_id}] runtime error: {e}");
            });
            verify_result(&result, spec, test_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Harness entry points (one per optimization level)
// ---------------------------------------------------------------------------

/// Test function for O0 (no optimization)
fn fixture_test_o0(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O0);
    Ok(())
}

/// Test function for O2 (full optimization)
fn fixture_test_o2(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O2);
    Ok(())
}

/// Test function for O1 (development optimization)
/// Skipped by default locally. Runs in CI or when WADO_FULL_TEST=1 is set.
fn fixture_test_o1(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::O1);
    Ok(())
}

/// Test function for O3 (aggressive optimization)
/// Skipped by default locally. Runs in CI or when WADO_FULL_TEST=1 is set.
fn fixture_test_o3(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::O3);
    Ok(())
}

/// Test function for Os (size optimization)
/// Skipped by default locally. Runs in CI or when WADO_FULL_TEST=1 is set.
fn fixture_test_os(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::Os);
    Ok(())
}

datatest_mini::harness! {
    // Pattern matches .wado files directly in fixtures/ but not in subdirectories
    // (subdirectories contain helper modules that are imported, not run as tests)
    { test = fixture_test_o0, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o1, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o2, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o3, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_os, root = "tests/fixtures", pattern = r"^[^/]+\.wado$" },
}
