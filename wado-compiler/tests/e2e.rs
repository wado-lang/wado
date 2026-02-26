//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them,
//! verifying the output matches expected values defined in each file's __DATA__ section.
//!
//! Test fixtures specify the target world via the top-level key in __DATA__:
//! - (default) no world key — runs as `wasi:cli/command`, checks stdout/stderr
//! - `"test": {}` — runs as test world, executes test block exports
//! - `"wasi:http/service": {...}` — runs as HTTP service with request/response spec
//!
//! Helper modules that are imported by tests go in subdirectories
//! (e.g., fixtures/sub/) and are not run as tests themselves.

mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
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

fn http_method_default() -> String {
    "GET".to_string()
}

fn http_path_default() -> String {
    "/".to_string()
}

/// HTTP request input for `"wasi:http/service"` tests
#[derive(Debug, Deserialize)]
struct HttpRequestSpec {
    /// HTTP method (default: `"GET"`)
    #[serde(default = "http_method_default")]
    method: String,

    /// Request path + query string, e.g. `"/hello?name=Alice"` (default: `"/"`)
    #[serde(default = "http_path_default")]
    path: String,

    /// Request headers as `[[name, value], ...]`
    #[serde(default)]
    headers: Vec<[String; 2]>,

    /// Request body as UTF-8 string (default: empty)
    #[serde(default)]
    body: Option<String>,
}

impl Default for HttpRequestSpec {
    fn default() -> Self {
        Self {
            method: http_method_default(),
            path: http_path_default(),
            headers: Vec::new(),
            body: None,
        }
    }
}

/// Expected HTTP response and injected request for `"wasi:http/service"` tests.
///
/// Example:
/// ```json
/// {
///   "wasi:http/service": {
///     "request": { "method": "POST", "path": "/api", "body": "hello" },
///     "status": 200,
///     "body": "world",
///     "headers_contain": [["content-type", "text/plain"]]
///   }
/// }
/// ```
#[derive(Debug, Deserialize, Default)]
struct HttpServiceSpec {
    /// Injected HTTP request (defaults to `GET /`)
    #[serde(default)]
    request: HttpRequestSpec,

    /// Expected HTTP status code
    #[serde(default)]
    status: Option<u16>,

    /// Expected response body (exact UTF-8 match)
    #[serde(default)]
    body: Option<String>,

    /// Strings that must appear in the response body
    #[serde(default)]
    body_contains: Vec<String>,

    /// Response headers that must be present with the given value: `[[name, value], ...]`
    #[serde(default)]
    headers_contain: Vec<[String; 2]>,
}

/// Test world spec for `"test": {}` — no sub-fields, presence signals the test world.
#[derive(Debug, Deserialize, Default)]
struct TestWorldSpec {}

/// Expected test results from __DATA__ section (JSON format)
#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    /// Test world: presence of this key (e.g. `"test": {}`) runs test block exports.
    #[serde(rename = "test")]
    #[serde(default)]
    test_world: Option<TestWorldSpec>,

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

    /// Preopened directories for WASI filesystem tests.
    /// Each entry is `[host_path, guest_path]`.
    /// Paths are relative to the workspace root (cargo test working directory).
    #[serde(default)]
    preopened_dirs: Vec<[String; 2]>,

    /// Stdin data to pipe into the program.
    /// If set, the program's stdin will receive this UTF-8 string.
    #[serde(default)]
    stdin: Option<String>,

    /// HTTP world spec: request injection + response expectations.
    /// Presence of this key implies `world = "wasi:http/service"`.
    #[serde(rename = "wasi:http/service")]
    http_service: Option<HttpServiceSpec>,

    // --- WIR pattern expectations (per optimization level) ---
    /// Patterns that must appear in WIR output at -O0
    #[serde(rename = "wir_expect:O0", default)]
    wir_expect_o0: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O0
    #[serde(rename = "wir_not_expect:O0", default)]
    wir_not_expect_o0: Vec<String>,

    /// Patterns that must appear in WIR output at -O1
    #[serde(rename = "wir_expect:O1", default)]
    wir_expect_o1: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O1
    #[serde(rename = "wir_not_expect:O1", default)]
    wir_not_expect_o1: Vec<String>,

    /// Patterns that must appear in WIR output at -O2
    #[serde(rename = "wir_expect:O2", default)]
    wir_expect_o2: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O2
    #[serde(rename = "wir_not_expect:O2", default)]
    wir_not_expect_o2: Vec<String>,

    /// Patterns that must appear in WIR output at -O3
    #[serde(rename = "wir_expect:O3", default)]
    wir_expect_o3: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O3
    #[serde(rename = "wir_not_expect:O3", default)]
    wir_not_expect_o3: Vec<String>,

    /// Patterns that must appear in WIR output at -Os
    #[serde(rename = "wir_expect:Os", default)]
    wir_expect_os: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -Os
    #[serde(rename = "wir_not_expect:Os", default)]
    wir_not_expect_os: Vec<String>,
}

impl TestSpec {
    /// Get WIR expect/not-expect patterns for a given optimization level.
    /// Returns `(expect, not_expect)` slices.
    fn wir_expectations(&self, opt_level: OptLevel) -> (&[String], &[String]) {
        match opt_level {
            OptLevel::O0 => (&self.wir_expect_o0, &self.wir_not_expect_o0),
            OptLevel::O1 => (&self.wir_expect_o1, &self.wir_not_expect_o1),
            OptLevel::O2 => (&self.wir_expect_o2, &self.wir_not_expect_o2),
            OptLevel::O3 => (&self.wir_expect_o3, &self.wir_not_expect_o3),
            OptLevel::Os => (&self.wir_expect_os, &self.wir_not_expect_os),
        }
    }

    /// Whether this test has any WIR expectations for the given optimization level.
    fn has_wir_expectations(&self, opt_level: OptLevel) -> bool {
        let (expect, not_expect) = self.wir_expectations(opt_level);
        !expect.is_empty() || !not_expect.is_empty()
    }
}

// ---------------------------------------------------------------------------
// CLI world verification (shared with test world)
// ---------------------------------------------------------------------------

/// Verify the actual result matches the expected spec
fn verify_result(result: &common::WasmRunResult, spec: &TestSpec, fixture_name: &str) {
    // Check trapped status
    assert_eq!(
        result.trapped, spec.trapped,
        "[{fixture_name}] trapped mismatch: expected {}, got {}\n  stderr: {:?}\n  stdout: {:?}",
        spec.trapped, result.trapped, result.stderr, result.stdout
    );

    // Check stdout exact match if specified
    if let Some(expected_stdout) = &spec.stdout {
        assert_eq!(
            &result.stdout, expected_stdout,
            "[{fixture_name}] stdout mismatch\n  stderr was: {:?}",
            result.stderr
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
    headers: http::HeaderMap,
    body: Vec<u8>,
}

/// Run an HTTP request against a compiled Wasm component
fn run_http_request(wasm: Vec<u8>, req_spec: &HttpRequestSpec) -> anyhow::Result<HttpTestResult> {
    http_runtime().block_on(run_http_request_async(wasm, req_spec))
}

async fn run_http_request_async(
    wasm: Vec<u8>,
    req_spec: &HttpRequestSpec,
) -> anyhow::Result<HttpTestResult> {
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

    // Build request from spec
    let method = req_spec
        .method
        .parse::<http::Method>()
        .unwrap_or(http::Method::GET);
    let uri = format!("http://localhost{}", req_spec.path);
    let body_bytes = req_spec.body.as_deref().unwrap_or("").as_bytes().to_vec();

    let mut builder = http::Request::builder().uri(uri).method(method);
    for [name, value] in &req_spec.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let http_req = builder.body(Full::new(Bytes::from(body_bytes)))?;

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
            let headers = res.headers().clone();
            let body = res.into_body().collect().await?.to_bytes().to_vec();
            Ok(HttpTestResult {
                status,
                headers,
                body,
            })
        }
        Err(Some(error_code)) => Ok(HttpTestResult {
            status: 500,
            headers: http::HeaderMap::new(),
            body: format!("{error_code:?}").into_bytes(),
        }),
        Err(None) => Err(anyhow::anyhow!("Handler returned error without error code")),
    }
}

/// Verify an HTTP test result matches the spec
fn verify_http_result(result: &HttpTestResult, spec: &HttpServiceSpec, fixture_name: &str) {
    if let Some(expected_status) = spec.status {
        assert_eq!(
            result.status, expected_status,
            "[{fixture_name}] HTTP status mismatch: expected {expected_status}, got {}",
            result.status
        );
    }

    if let Some(expected_body) = &spec.body {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert_eq!(
            actual_body,
            expected_body.as_str(),
            "[{fixture_name}] body mismatch"
        );
    }

    for expected in &spec.body_contains {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert!(
            actual_body.contains(expected.as_str()),
            "[{fixture_name}] body should contain '{expected}', but got:\n{actual_body}"
        );
    }

    for [name, value] in &spec.headers_contain {
        let header_name = http::header::HeaderName::from_bytes(name.as_bytes())
            .unwrap_or_else(|_| panic!("[{fixture_name}] invalid header name in spec: {name}"));
        let expected_val = value.as_str();
        let actual_val = result
            .headers
            .get(&header_name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(missing)");
        assert_eq!(
            actual_val, expected_val,
            "[{fixture_name}] response header '{name}': expected '{expected_val}', got '{actual_val}'"
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
            let expect_trap = test_name.starts_with("test-trap-");
            let is_todo = test_name.starts_with("test-todo-");

            let stdout_pipe = MemoryOutputPipe::new(65536);
            let stdout_clone = stdout_pipe.clone();
            let stderr_pipe = MemoryOutputPipe::new(65536);
            let stderr_clone = stderr_pipe.clone();

            let state = common::CliWasiState::new_with_pipes(stdout_pipe, stderr_pipe);
            let mut store = Store::new(engine, state);

            let instance = linker.instantiate_async(&mut store, &component).await?;
            let func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, test_name)?;

            match func.call_async(&mut store, ()).await {
                Ok((Ok(()),)) => {
                    if is_todo {
                        anyhow::bail!(
                            "[{test_id}] TODO test '{test_name}' passed unexpectedly — \
                             the feature may be implemented; remove the #[TODO] attribute"
                        );
                    } else if expect_trap {
                        anyhow::bail!(
                            "[{test_id}] test '{test_name}' was expected to trap but returned Ok(())"
                        );
                    }
                    // passed
                }
                Ok((Err(()),)) => {
                    let stderr_text = String::from_utf8_lossy(&stderr_clone.contents()).to_string();
                    anyhow::bail!(
                        "[{test_id}] test '{test_name}' returned error. stderr: {stderr_text}"
                    );
                }
                Err(e) => {
                    if !expect_trap && !is_todo {
                        let stderr_text =
                            String::from_utf8_lossy(&stderr_clone.contents()).to_string();
                        anyhow::bail!(
                            "[{test_id}] test '{test_name}' trapped: {e}. stderr: {stderr_text}"
                        );
                    }
                    // expected trap (expect_trap or TODO): pass
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
    // Determine target world from the world key present in the spec
    let target_world = if spec.http_service.is_some() {
        Some("wasi:http/service".to_string())
    } else if spec.test_world.is_some() {
        Some("test".to_string())
    } else {
        None
    };

    // Use CompilerOptions to pass the target world through
    let has_wir = spec.has_wir_expectations(opt_level);
    let options = CompilerOptions {
        opt_level,
        target_world,
        skip_validation: false,
        retain_wir: has_wir,
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
    if let Some(http_spec) = &spec.http_service {
        match run_http_request(compile_result.wasm, &http_spec.request) {
            Ok(result) => {
                assert!(
                    !spec.trapped,
                    "[{test_id}] expected HTTP handler to trap, but request succeeded with status {}",
                    result.status
                );
                verify_http_result(&result, http_spec, test_id);
            }
            Err(e) => {
                assert!(
                    spec.trapped,
                    "[{test_id}] HTTP error (no trap expected): {e:?}"
                );
            }
        }
    } else if spec.test_world.is_some() {
        let result = run_test_world(&compile_result.wasm, test_id).unwrap_or_else(|e| {
            panic!("[{test_id}] test world error: {e:?}");
        });
        verify_result(&result, spec, test_id);
    } else {
        // Default: wasi:cli/command
        let dirs: Vec<(String, String)> = spec
            .preopened_dirs
            .iter()
            .map(|[h, g]| (h.clone(), g.clone()))
            .collect();
        let result =
            common::run_wasm_with_options(compile_result.wasm, &dirs, spec.stdin.as_deref())
                .unwrap_or_else(|e| {
                    panic!("[{test_id}] runtime error: {e}");
                });
        verify_result(&result, spec, test_id);
    }

    // Verify WIR pattern expectations (if any for this optimization level)
    if has_wir {
        let wir_module = compile_result
            .wir_module
            .as_ref()
            .expect("wir_module should be retained when retain_wir is set");
        let filename = fixture_path.to_string_lossy();
        let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_module, Some(&filename));

        let (expect, not_expect) = spec.wir_expectations(opt_level);
        let opt_name = common::opt_level_name(opt_level);

        for pattern in expect {
            assert!(
                wir_text.contains(pattern),
                "[{test_id}] wir_expect:{opt_name} failed: pattern not found in WIR\n\
                 pattern: {pattern}\n\
                 WIR output:\n{wir_text}"
            );
        }

        for pattern in not_expect {
            assert!(
                !wir_text.contains(pattern),
                "[{test_id}] wir_not_expect:{opt_name} failed: pattern unexpectedly found in WIR\n\
                 pattern: {pattern}\n\
                 WIR output:\n{wir_text}"
            );
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
