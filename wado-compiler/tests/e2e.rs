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
use futures::future::{Either, select};
use http_body_util::{BodyExt, Full};
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::Store;
use wasmtime::component::{Component, ResourceTable};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::p3::Request;
use wasmtime_wasi_http::p3::bindings::Service;

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

    /// Preopened directories for WASI filesystem tests.
    /// Each entry is `[host_path, guest_path]`.
    /// Paths are relative to the workspace root (cargo test working directory).
    #[serde(default)]
    preopened_dirs: Vec<[String; 2]>,

    /// Skip this test under `-Os` (e.g. tests that rely on debug names).
    #[serde(default)]
    skip_os: bool,

    /// Run this test only at the listed optimization levels (e.g. `["O3"]`).
    /// When set, all other levels are skipped.
    #[serde(default)]
    only_opt: Vec<String>,

    /// Stdin data to pipe into the program.
    /// If set, the program's stdin will receive this UTF-8 string.
    #[serde(default)]
    stdin: Option<String>,

    /// HTTP world spec: request injection + response expectations.
    /// Presence of this key implies `world = "wasi:http/service"`.
    #[serde(rename = "wasi:http/service")]
    http_service: Option<HttpServiceSpec>,

    /// Override the allocator mode: "bump" or "debug".
    /// If not set, auto-selects based on target world (debug for test world, bump otherwise).
    #[serde(default)]
    allocator: Option<String>,

    /// Mock responses for outgoing HTTP requests (keyed by URL or path).
    /// When present, any `wasi:http/client#send` from the guest will be
    /// intercepted and matched against these entries.
    #[serde(default)]
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,

    /// Mock responses for `wasi:tls` handshakes (keyed by `server_name`).
    /// Any `Connector::connect(_, server_name)` is matched against these
    /// entries. Unmatched server names fail the handshake with a clear error
    /// so tests cannot silently reach the real network.
    #[serde(default)]
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,

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
fn run_http_request(
    wasm: Vec<u8>,
    req_spec: &HttpRequestSpec,
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,
) -> anyhow::Result<HttpTestResult> {
    http_runtime().block_on(run_http_request_async(
        wasm,
        req_spec,
        outgoing_mocks,
        tls_mocks,
    ))
}

async fn run_http_request_async(
    wasm: Vec<u8>,
    req_spec: &HttpRequestSpec,
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,
) -> anyhow::Result<HttpTestResult> {
    let engine = common::engine();
    let component = Component::new(engine, &wasm)
        .map_err(|e| anyhow::anyhow!("failed to create component: {e:?}"))?;

    let linker = common::linker(engine)?;

    let state = common::WasiState {
        ctx: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
        http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
        http_hooks: common::TestHttpCtx {
            mocks: outgoing_mocks,
        },
        tls_ctx: common::build_tls_ctx(tls_mocks),
    };
    let mut store = Store::new(engine, state);
    // Set epoch deadline for timeout enforcement (HTTP tests use 5s default)
    let deadline_ticks = (common::DEFAULT_TIMEOUT_MS / 1000).max(1);
    store.set_epoch_deadline(deadline_ticks);

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

    let timeout_duration = Duration::from_secs(1);

    // Run handler, request body I/O, and response body collection all inside
    // run_concurrent. This follows wasmtime's own HTTP test pattern
    // (vendor/wasmtime/crates/wasi-http/tests/all/p3/mod.rs::run_http).
    // Everything must be inside run_concurrent so that async host futures
    // (e.g., Client::send mock responses) can be polled by the runtime.
    let handle_result = tokio::time::timeout(timeout_duration, async {
        store
            .run_concurrent(async |store| {
                use std::pin::pin;

                let handler = pin!(async {
                    let res = match service.handle(store, req).await? {
                        Ok(res) => res,
                        Err(err) => return anyhow::Ok(Err(Some(err))),
                    };
                    let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                    let (parts, body) = res.into_parts();
                    let body = body
                        .collect()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to collect body: {e}"))?;
                    anyhow::Ok(Ok(http::Response::from_parts(parts, body)))
                });
                let io = pin!(async {
                    io.await
                        .map_err(|e| anyhow::anyhow!("request body I/O: {e}"))
                });
                // select: handler completing first is normal (guest may not
                // consume request body); io completing first means body error.
                match select(handler, io).await {
                    Either::Left((result, _)) => result,
                    Either::Right((result, _)) => result.map(|()| Err(None)),
                }
            })
            .await?
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP handler timed out after {timeout_duration:?}"))??;

    match handle_result {
        Ok(res) => {
            let status = res.status().as_u16();
            let headers = res.headers().clone();
            let body = res.into_body().to_bytes().to_vec();
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
            result.status,
            expected_status,
            "[{fixture_name}] HTTP status mismatch: expected {expected_status}, got {}; body: {}",
            result.status,
            String::from_utf8_lossy(&result.body),
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

/// Sentinel panic payload signalling that a `#[TODO]` test passed unexpectedly.
///
/// The test-world runner raises this via `std::panic::panic_any` (rather than
/// `anyhow::bail!`) so the outer `#![TODO]` module wrapper can distinguish a
/// resolved-but-still-marked TODO (hard failure) from a genuine pending test
/// (silent OK). The runner identifies it by downcasting the panic payload;
/// all other panics fall back to the "pending" path.
#[derive(Debug)]
struct TodoResolved(String);

/// Run all test exports from a Wasm component compiled with the `test` world.
/// Each test function is called in its own Store. All tests must pass.
fn run_test_world(
    wasm: &[u8],
    test_id: &str,
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,
) -> anyhow::Result<common::WasmRunResult> {
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    let rt = common::runtime();
    let engine = common::engine();

    rt.block_on(async {
        let component = Component::new(engine, wasm)?;
        let linker = common::linker(engine)?;

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

            let mut state = common::WasiState::new_with_pipes(stdout_pipe, stderr_pipe);
            state.http_hooks.mocks = outgoing_mocks.clone();
            state.tls_ctx = common::build_tls_ctx(tls_mocks.clone());
            let mut store = Store::new(engine, state);
            // Parse per-test timeout from export name (e.g., "test-tm2000-0-slow")
            // and set epoch deadline for timeout enforcement
            let timeout_ms =
                parse_test_timeout_ms(test_name).unwrap_or(common::DEFAULT_TIMEOUT_MS);
            let deadline_ticks = (timeout_ms / 1000).max(1);
            store.set_epoch_deadline(deadline_ticks);

            let instance = linker.instantiate_async(&mut store, &component).await?;
            let func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, test_name)?;

            match func.call_async(&mut store, ()).await {
                Ok((Ok(()),)) => {
                    if is_todo {
                        // TODO test passed — the feature may now work. This must surface
                        // as a hard failure even inside an outer `#![TODO]` catch_unwind,
                        // so we panic with a typed sentinel rather than bailing through
                        // the generic `anyhow::Result` channel.
                        std::panic::panic_any(TodoResolved(format!(
                            "[{test_id}] TODO test '{test_name}' resolved — \
                             remove the #[TODO] attribute"
                        )));
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
                    // expected trap (expect_trap or TODO): pending
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

/// Parse per-test timeout from export name (e.g., "test-tm2000-0-slow" → Some(2000)).
fn parse_test_timeout_ms(test_name: &str) -> Option<u64> {
    let rest = test_name
        .strip_prefix("test-trap-")
        .or_else(|| test_name.strip_prefix("test-todo-"))
        .or_else(|| test_name.strip_prefix("test-"))?;
    let rest = rest.strip_prefix("tm")?;
    let end = rest.find('-')?;
    rest[..end].parse::<u64>().ok()
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

    // Skip if the spec says skip_os and we're running -Os
    if spec.skip_os && opt_level == OptLevel::Os {
        eprintln!("[{test_id}] skipped (skip_os)");
        return;
    }

    // Skip if only_opt is set and this level is not in the list
    if !spec.only_opt.is_empty() {
        let level_str = match opt_level {
            OptLevel::O0 => "O0",
            OptLevel::O1 => "O1",
            OptLevel::O2 => "O2",
            OptLevel::O3 => "O3",
            OptLevel::Os => "Os",
        };
        if !spec.only_opt.iter().any(|s| s == level_str) {
            eprintln!("[{test_id}] skipped (only_opt: {:?})", spec.only_opt);
            return;
        }
    }

    // Check if source has #![TODO] to determine panic recovery strategy.
    // For test world: individual tests are already marked #[TODO] by the resolver,
    // so no special handling is needed here.
    // For CLI/HTTP worlds: runtime failures in #![TODO] modules need catch_unwind.
    let is_todo_module = match wado_compiler::parse(source) {
        Ok(r) => r.ast.has_todo(),
        Err(e) => e.is_todo_module(),
    };

    if is_todo_module {
        eprintln!("[{test_id}] #![TODO] module — expecting failure");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
        }));
        match test_result {
            Ok(()) if spec.test_world.is_some() => {
                // Test world: every test is marked `test-todo-*` by the resolver.
                // Reaching Ok means each one trapped as expected — pending, not resolved.
            }
            Ok(()) => {
                // #![TODO] module passed — the feature may now work.
                // This is a hard failure: #![TODO] must be removed.
                panic!(
                    "[{test_id}] #![TODO] module resolved — \
                     remove the #![TODO] attribute from the source file."
                );
            }
            Err(err) => {
                // A `test-todo-*` export that returned Ok signals a resolved TODO via
                // `TodoResolved`. Re-raise it so the test framework reports the failure;
                // everything else is treated as a still-pending TODO.
                if let Some(resolved) = err.downcast_ref::<TodoResolved>() {
                    panic!("{}", resolved.0);
                }
                let msg = err
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| err.downcast_ref::<&str>().copied())
                    .unwrap_or("(unknown panic)");
                eprintln!("[{test_id}] #![TODO] module pending: {msg}");
                return;
            }
        }
    }

    // Normal test (or resolved TODO module) - run without panic recovery
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
    // Default to debug allocator in e2e tests to catch use-after-free bugs,
    // unless the fixture explicitly overrides it.
    let allocator = Some(
        spec.allocator
            .clone()
            .unwrap_or_else(|| "debug".to_string()),
    );
    let options = CompilerOptions {
        opt_level,
        target_world,
        skip_validation: false,
        retain_wir: has_wir,
        inline_threshold: None,
        opt_iterations: None,
        allocator,
        ..Default::default()
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

    // Optional: dump the compiled wasm (and a wat next to it) so the
    // bytes the e2e runner produces can be diffed against `wado compile`.
    // Set `WADO_KEEP_WASM_DIR=/tmp/wado-debug` (directory will be created
    // if missing). Filenames are `<fixture_name>.<opt>.{wasm,wat}`.
    if let Ok(dir) = std::env::var("WADO_KEEP_WASM_DIR") {
        let fixture_name = fixture_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let opt_name = common::opt_level_name(opt_level);
        keep_wasm_artifacts(&dir, &fixture_name, opt_name, &compile_result.wasm, test_id);
    }

    // Dispatch to the appropriate runner based on world
    if let Some(http_spec) = &spec.http_service {
        match run_http_request(
            compile_result.wasm,
            &http_spec.request,
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        ) {
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
        let result = run_test_world(
            &compile_result.wasm,
            test_id,
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        )
        .unwrap_or_else(|e| {
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
        let result = common::run_wasm_with_full_options(
            compile_result.wasm,
            &dirs,
            spec.stdin.as_deref(),
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        )
        .unwrap_or_else(|e| {
            panic!("[{test_id}] runtime error: {e}");
        });
        verify_result(&result, spec, test_id);
    }

    // Verify WIR pattern expectations (if any for this optimization level)
    if has_wir {
        let wir_package = compile_result
            .wir_package
            .as_ref()
            .expect("wir_package should be retained when retain_wir is set");
        let filename = fixture_path.to_string_lossy();
        let wir_text = wado_compiler::wir_unparse::unparse_wir(wir_package, Some(&filename));

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
/// Skipped by default locally. Runs in CI or when `WADO_FULL_TEST=1` is set.
fn fixture_test_o1(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::O1);
    Ok(())
}

/// Test function for O3 (aggressive optimization)
/// Skipped by default locally. Runs in CI or when `WADO_FULL_TEST=1` is set.
fn fixture_test_o3(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::O3);
    Ok(())
}

/// Test function for Os (size optimization)
/// Skipped by default locally. Runs in CI or when `WADO_FULL_TEST=1` is set.
fn fixture_test_os(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CI").is_err() && std::env::var("WADO_FULL_TEST").is_err() {
        return Ok(()); // Skip locally by default
    }
    run_fixture_test_with_opt(path, content, OptLevel::Os);
    Ok(())
}

/// Write the compiled wasm (and a wat decoded from it) under `dir` so it can
/// be inspected and diffed against `wado compile`. Activated by setting the
/// `WADO_KEEP_WASM_DIR` environment variable. Failures are reported via
/// `eprintln!` and never block the test.
fn keep_wasm_artifacts(dir: &str, fixture_name: &str, opt_name: &str, wasm: &[u8], test_id: &str) {
    let dir_path = std::path::Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(dir_path) {
        eprintln!("[{test_id}] WADO_KEEP_WASM_DIR: cannot create '{dir}': {e}");
        return;
    }
    let stem = fixture_name.strip_suffix(".wado").unwrap_or(fixture_name);
    let wasm_path = dir_path.join(format!("{stem}.{opt_name}.wasm"));
    if let Err(e) = std::fs::write(&wasm_path, wasm) {
        eprintln!(
            "[{test_id}] WADO_KEEP_WASM_DIR: cannot write {}: {e}",
            wasm_path.display()
        );
        return;
    }
    let wat_path = dir_path.join(format!("{stem}.{opt_name}.wat"));
    match wasmprinter::print_bytes(wasm) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&wat_path, text) {
                eprintln!(
                    "[{test_id}] WADO_KEEP_WASM_DIR: cannot write {}: {e}",
                    wat_path.display()
                );
            }
        }
        Err(e) => {
            eprintln!("[{test_id}] WADO_KEEP_WASM_DIR: wasmprinter failed: {e}");
        }
    }
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

/// End-to-end check on the `#![TODO]` wrapper: a module-level `#![TODO]` whose
/// test passes must still hard-fail. Before the sentinel fix the resolved
/// panic was swallowed as "pending" because the outer `catch_unwind` could
/// not distinguish it from a genuine still-pending TODO trap.
#[test]
#[should_panic(expected = "TODO test 'test-todo-0-passes-unexpectedly' resolved")]
fn module_todo_with_passing_test_hard_fails() {
    let source = r#"#![TODO]
test "passes unexpectedly" {
    assert true;
}

__DATA__
{"test": {}}
"#;
    run_fixture_test_with_opt(
        std::path::Path::new("synthetic_module_todo.wado"),
        source,
        OptLevel::O0,
    );
}

/// A `#[TODO]` test that returns Ok must surface as a hard failure rather than
/// being swallowed as "pending" by the outer `#![TODO]` `catch_unwind` path.
/// We exercise the runner end-to-end: compile a synthetic source whose only
/// test is `#[TODO]` and passes, then call `run_test_world` and assert that
/// the resulting panic payload carries the `TodoResolved` sentinel.
#[test]
fn run_test_world_signals_resolved_todo_via_sentinel() {
    let source = r#"#[TODO]
test "passes unexpectedly" {
    assert true;
}
"#;
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        target_world: Some("test".to_string()),
        ..Default::default()
    };
    let path = std::path::Path::new("synthetic_todo_resolved.wado");
    let compile_result = common::compile_source_with_compiler_options(path, source, options)
        .expect("compilation should succeed");

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_test_world(
            &compile_result.wasm,
            "synthetic-todo-resolved",
            indexmap::IndexMap::new(),
            indexmap::IndexMap::new(),
        )
    }))
    .expect_err("a resolved #[TODO] test must panic, not return");

    let resolved = payload
        .downcast_ref::<TodoResolved>()
        .expect("panic payload should be TodoResolved");
    assert!(
        resolved.0.contains("resolved"),
        "message should mention `resolved`: {}",
        resolved.0,
    );
    assert!(
        resolved.0.contains("passes-unexpectedly"),
        "message should mention the offending test name: {}",
        resolved.0,
    );
}
// touched to trigger datatest_mini rediscovery
